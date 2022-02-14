use std::collections::BTreeMap;

use postgres_types::Type;

use crate::proto::metrics::pb::{dimension, measurement, Datum, Dimension, Measurement};

pub struct TypeConverter {
    pub statistic_set_type: Type,
    pub histogram_type: Type,
}

impl TypeConverter {
    pub fn measurement_sql_type(&self, measurement: &Measurement) -> Option<Type> {
        measurement.value.as_ref().map(|v| match v {
            measurement::Value::Inumber(_) => Type::INT8,
            measurement::Value::Fnumber(_) => Type::FLOAT8,
            measurement::Value::StatisticSet(_) => self.statistic_set_type.clone(),
            measurement::Value::Histogram(_) => Type::JSONB,
        })
    }

    pub fn dimension_sql_type(&self, dimension: &Dimension) -> Option<Type> {
        dimension.value.as_ref().map(|v| match v {
            dimension::Value::String(_) => Type::TEXT,
            dimension::Value::Number(_) => Type::INT8,
            dimension::Value::Boolean(_) => Type::BOOL,
        })
    }

    pub fn get_dimension_type_map(&self, datums: &[&Datum]) -> BTreeMap<String, Type> {
        datums
            .iter()
            .map(|d| d.dimensions.iter())
            .flatten()
            .filter_map(|(dimension_name, dimension_value)| {
                self.dimension_sql_type(dimension_value)
                    .map(|sql_type| (dimension_name.clone(), sql_type))
            })
            .collect()
    }

    pub fn get_measurement_type_map(&self, datums: &[&Datum]) -> BTreeMap<String, Type> {
        datums
            .iter()
            .map(|d| d.measurements.iter())
            .flatten()
            .filter_map(|(measurement_name, measurement_value)| {
                self.measurement_sql_type(measurement_value)
                    .map(|sql_type| (measurement_name.clone(), sql_type))
            })
            .collect()
    }
}
