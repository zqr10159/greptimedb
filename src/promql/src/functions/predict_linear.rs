// Copyright 2023 Greptime Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implementation of [`predict_linear`](https://prometheus.io/docs/prometheus/latest/querying/functions/#predict_linear) in PromQL. Refer to the [original
//! implementation](https://github.com/prometheus/prometheus/blob/90b2f7a540b8a70d8d81372e6692dcbb67ccbaaa/promql/functions.go#L859-L872).

use std::sync::Arc;

use datafusion::arrow::array::{Float64Array, TimestampMillisecondArray};
use datafusion::arrow::datatypes::TimeUnit;
use datafusion::common::DataFusionError;
use datafusion::logical_expr::{ScalarUDF, Volatility};
use datafusion::physical_plan::ColumnarValue;
use datafusion_common::ScalarValue;
use datafusion_expr::create_udf;
use datatypes::arrow::array::Array;
use datatypes::arrow::datatypes::DataType;

use crate::error;
use crate::functions::{extract_array, linear_regression};
use crate::range_array::RangeArray;

pub struct PredictLinear;

impl PredictLinear {
    pub const fn name() -> &'static str {
        "prom_predict_linear"
    }

    pub fn scalar_udf() -> ScalarUDF {
        let input_types = vec![
            // time index column
            RangeArray::convert_data_type(DataType::Timestamp(TimeUnit::Millisecond, None)),
            // value column
            RangeArray::convert_data_type(DataType::Float64),
            // t
            DataType::Int64,
        ];
        create_udf(
            Self::name(),
            input_types,
            DataType::Float64,
            Volatility::Volatile,
            Arc::new(Self::predict_linear) as _,
        )
    }

    fn predict_linear(input: &[ColumnarValue]) -> Result<ColumnarValue, DataFusionError> {
        error::ensure(
            input.len() == 3,
            DataFusionError::Plan("prom_predict_linear function should have 3 inputs".to_string()),
        )?;

        let ts_array = extract_array(&input[0])?;
        let value_array = extract_array(&input[1])?;
        let t_col = &input[2];

        let ts_range: RangeArray = RangeArray::try_new(ts_array.to_data().into())?;
        let value_range: RangeArray = RangeArray::try_new(value_array.to_data().into())?;
        error::ensure(
            ts_range.len() == value_range.len(),
            DataFusionError::Execution(format!(
                "{}: input arrays should have the same length, found {} and {}",
                Self::name(),
                ts_range.len(),
                value_range.len()
            )),
        )?;
        error::ensure(
            ts_range.value_type() == DataType::Timestamp(TimeUnit::Millisecond, None),
            DataFusionError::Execution(format!(
                "{}: expect TimestampMillisecond as time index array's type, found {}",
                Self::name(),
                ts_range.value_type()
            )),
        )?;
        error::ensure(
            value_range.value_type() == DataType::Float64,
            DataFusionError::Execution(format!(
                "{}: expect Float64 as value array's type, found {}",
                Self::name(),
                value_range.value_type()
            )),
        )?;

        let t_iter: Box<dyn Iterator<Item = Option<i64>>> = match t_col {
            ColumnarValue::Scalar(t_scalar) => {
                let t = if let ScalarValue::Int64(Some(t_val)) = t_scalar {
                    *t_val
                } else {
                    // For `ScalarValue::Int64(None)` or other scalar types, returns NULL array,
                    // which conforms to PromQL's behavior.
                    let null_array = Float64Array::new_null(ts_range.len());
                    return Ok(ColumnarValue::Array(Arc::new(null_array)));
                };
                Box::new((0..ts_range.len()).map(move |_| Some(t)))
            }
            ColumnarValue::Array(t_array) => {
                let t_array = t_array
                    .as_any()
                    .downcast_ref::<datafusion::arrow::array::Int64Array>()
                    .ok_or_else(|| {
                        DataFusionError::Execution(format!(
                            "{}: expect Int64 as t array's type, found {}",
                            Self::name(),
                            t_array.data_type()
                        ))
                    })?;
                error::ensure(
                    t_array.len() == ts_range.len(),
                    DataFusionError::Execution(format!(
                        "{}: t array should have the same length as other columns, found {} and {}",
                        Self::name(),
                        t_array.len(),
                        ts_range.len()
                    )),
                )?;

                Box::new(t_array.iter())
            }
        };
        let mut result_array = Vec::with_capacity(ts_range.len());
        for (index, t) in t_iter.enumerate() {
            let (timestamps, values) = get_ts_values(&ts_range, &value_range, index, Self::name())?;
            let ret = predict_linear_impl(&timestamps, &values, t.unwrap());
            result_array.push(ret);
        }

        let result = ColumnarValue::Array(Arc::new(Float64Array::from_iter(result_array)));
        Ok(result)
    }
}

fn get_ts_values(
    ts_range: &RangeArray,
    value_range: &RangeArray,
    index: usize,
    func_name: &str,
) -> Result<(TimestampMillisecondArray, Float64Array), DataFusionError> {
    let timestamps = ts_range
        .get(index)
        .unwrap()
        .as_any()
        .downcast_ref::<TimestampMillisecondArray>()
        .unwrap()
        .clone();
    let values = value_range
        .get(index)
        .unwrap()
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap()
        .clone();
    error::ensure(
        timestamps.len() == values.len(),
        DataFusionError::Execution(format!(
            "{}: time and value arrays in a group should have the same length, found {} and {}",
            func_name,
            timestamps.len(),
            values.len()
        )),
    )?;
    Ok((timestamps, values))
}

fn predict_linear_impl(
    timestamps: &TimestampMillisecondArray,
    values: &Float64Array,
    t: i64,
) -> Option<f64> {
    if timestamps.len() < 2 {
        return None;
    }

    // last timestamp is evaluation timestamp
    let evaluate_ts = timestamps.value(timestamps.len() - 1);
    let (slope, intercept) = linear_regression(timestamps, values, evaluate_ts);

    if slope.is_none() || intercept.is_none() {
        return None;
    }

    Some(slope.unwrap() * t as f64 + intercept.unwrap())
}

#[cfg(test)]
mod test {
    use std::vec;

    use super::*;
    use crate::functions::test_util::simple_range_udf_runner;

    // build timestamp range and value range arrays for test
    fn build_test_range_arrays() -> (RangeArray, RangeArray) {
        let ts_array = Arc::new(TimestampMillisecondArray::from_iter(
            [
                0i64, 300, 600, 900, 1200, 1500, 1800, 2100, 2400, 2700, 3000,
            ]
            .into_iter()
            .map(Some),
        ));
        let ranges = [(0, 11)];

        let values_array = Arc::new(Float64Array::from_iter([
            0.0, 10.0, 20.0, 30.0, 40.0, 0.0, 10.0, 20.0, 30.0, 40.0, 50.0,
        ]));

        let ts_range_array = RangeArray::from_ranges(ts_array, ranges).unwrap();
        let value_range_array = RangeArray::from_ranges(values_array, ranges).unwrap();

        (ts_range_array, value_range_array)
    }

    #[test]
    fn calculate_predict_linear_none() {
        let ts_array = Arc::new(TimestampMillisecondArray::from_iter(
            [0i64].into_iter().map(Some),
        ));
        let ranges = [(0, 0), (0, 1)];
        let values_array = Arc::new(Float64Array::from_iter([0.0]));
        let ts_array = RangeArray::from_ranges(ts_array, ranges).unwrap();
        let value_array = RangeArray::from_ranges(values_array, ranges).unwrap();
        simple_range_udf_runner(
            PredictLinear::scalar_udf(),
            ts_array,
            value_array,
            vec![ScalarValue::Int64(Some(0))],
            vec![None, None],
        );
    }

    #[test]
    fn calculate_predict_linear_test1() {
        let (ts_array, value_array) = build_test_range_arrays();
        simple_range_udf_runner(
            PredictLinear::scalar_udf(),
            ts_array,
            value_array,
            vec![ScalarValue::Int64(Some(0))],
            // value at t = 0
            vec![Some(38.63636363636364)],
        );
    }

    #[test]
    fn calculate_predict_linear_test2() {
        let (ts_array, value_array) = build_test_range_arrays();
        simple_range_udf_runner(
            PredictLinear::scalar_udf(),
            ts_array,
            value_array,
            vec![ScalarValue::Int64(Some(3000))],
            // value at t = 3000
            vec![Some(31856.818181818187)],
        );
    }

    #[test]
    fn calculate_predict_linear_test3() {
        let (ts_array, value_array) = build_test_range_arrays();
        simple_range_udf_runner(
            PredictLinear::scalar_udf(),
            ts_array,
            value_array,
            vec![ScalarValue::Int64(Some(4200))],
            // value at t = 4200
            vec![Some(44584.09090909091)],
        );
    }

    #[test]
    fn calculate_predict_linear_test4() {
        let (ts_array, value_array) = build_test_range_arrays();
        simple_range_udf_runner(
            PredictLinear::scalar_udf(),
            ts_array,
            value_array,
            vec![ScalarValue::Int64(Some(6600))],
            // value at t = 6600
            vec![Some(70038.63636363638)],
        );
    }

    #[test]
    fn calculate_predict_linear_test5() {
        let (ts_array, value_array) = build_test_range_arrays();
        simple_range_udf_runner(
            PredictLinear::scalar_udf(),
            ts_array,
            value_array,
            vec![ScalarValue::Int64(Some(7800))],
            // value at t = 7800
            vec![Some(82765.9090909091)],
        );
    }
}
