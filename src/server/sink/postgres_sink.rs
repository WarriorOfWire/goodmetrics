use std::{
    collections::BTreeMap,
    error::Error,
    fmt::Display,
    rc::Rc,
    time::{Duration, SystemTime},
};

use crate::{
    postgres_things::{
        ddl::{self, clean_id},
        histogram::get_or_create_histogram_type,
        postgres_connector::PostgresConnector,
        statistic_set::get_or_create_statistic_set_type,
        type_conversion::TypeConverter,
    },
    proto::metrics::pb::{dimension, measurement, Datum, Dimension, Measurement},
};
use bb8::PooledConnection;
use bb8_postgres::PostgresConnectionManager;
use futures::pin_mut;
use itertools::Itertools;
use lazy_static::lazy_static;
use regex::Regex;
use thiserror::Error;
use tokio::task;
use tokio_postgres::{
    binary_copy::BinaryCopyInWriter,
    error::SqlState,
    types::{ToSql, Type, WrongType},
    CopyInSink, GenericClient, NoTls,
};

use super::metricssendqueue::MetricsReceiveQueue;

#[derive(Debug, Error)]
pub struct DescribedError {
    pub message: String,
    pub inner: tokio_postgres::Error,
}

impl Display for DescribedError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("DescribedError")
            .field("message", &self.message)
            .field("cause", &self.inner)
            .finish()
    }
}

#[derive(Debug, Error)]
pub struct StringError {
    pub message: String,
}

impl Display for StringError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("StringError")
            .field("message", &self.message)
            .finish()
    }
}

#[derive(Debug, Error)]
pub struct MissingTable {
    pub table: String,
}

impl Display for MissingTable {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("MissingTable")
            .field("table", &self.table)
            .finish()
    }
}

#[derive(Debug, Error)]
pub struct MissingColumn {
    pub table: String,
    pub column: String,
    pub data_type: String,
}

impl Display for MissingColumn {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("MissingColumn")
            .field("table", &self.table)
            .field("column", &self.column)
            .finish()
    }
}

#[derive(Error, Debug)]
pub enum SinkError {
    #[error("unhandled postgres error")]
    Postgres(#[from] tokio_postgres::Error),

    #[error("Some postgres error with a description")]
    DescribedError(#[from] DescribedError),

    #[error("unhandled error")]
    StringError(#[from] StringError),

    #[error("i gotta have more column")]
    MissingColumn(#[from] MissingColumn),

    #[error("i gotta have more table")]
    MissingTable(#[from] MissingTable),
}

lazy_static! {
    // column "available_messages" of relation "table_name" does not exist
    static ref UNDEFINED_COLUMN: Regex = Regex::new(r#"column "(?P<column>.+)" of relation "(?P<table>.+)" does not exist"#).unwrap();
    static ref UNDEFINED_TABLE: Regex = Regex::new(r#"relation "(?P<table>.+)" does not exist"#).unwrap();
}

pub struct PostgresSender {
    connector: PostgresConnector,
    rx: MetricsReceiveQueue,
    type_converter: TypeConverter,
}

impl PostgresSender {
    pub async fn new_connection(
        connection_string: &str,
        rx: MetricsReceiveQueue,
    ) -> Result<PostgresSender, SinkError> {
        log::debug!("new_connection: {:?}", connection_string);
        let max_conns = 16;
        let mut connector =
            PostgresConnector::new(connection_string.to_string(), max_conns).await?;

        let type_converter = {
            let statistic_set_type = get_or_create_statistic_set_type(&mut connector).await?;
            let histogram_type = get_or_create_histogram_type(&mut connector).await?;
            TypeConverter {
                statistic_set_type,
                histogram_type,
            }
        };

        Ok(PostgresSender {
            connector,
            rx,
            type_converter,
        })
    }

    pub async fn consume_stuff(mut self) -> Result<u32, SinkError> {
        log::info!("started consumer");
        let connector = Rc::new(self.connector);
        let type_converter = Rc::new(self.type_converter);

        while let Some(mut batch) = self.rx.recv().await {
            log::info!("Sender woke. Trying to collect a batch...");
            tokio::time::sleep(Duration::from_secs(5)).await;
            let mut api_calls = 1;
            while let Ok(mut extras) = self.rx.rx.try_recv() {
                api_calls += 1;
                batch.append(&mut extras);
            }

            let batch_tasks = task::LocalSet::new();

            let batch_connector = connector.clone();
            let batch_type_converter = type_converter.clone();
            batch_tasks
                .run_until(async move {
                    let batchlen = batch.len();
                    let grouped_metrics = group_metrics(batch);
                    log::info!(
                        "Sending some metrics. batch size: {}, metrics: {}, api calls: {}",
                        batchlen,
                        grouped_metrics.len(),
                        api_calls,
                    );

                    for (metric, datums) in grouped_metrics.into_iter() {
                        task::spawn_local(PostgresSender::send_some(
                            batch_connector.clone(),
                            batch_type_converter.clone(),
                            metric,
                            datums,
                        ));
                    }
                })
                .await;

            batch_tasks.await;
        }
        log::info!("ended consumer");
        Ok(1)
    }

    async fn send_some(
        connector: Rc<PostgresConnector>,
        type_converter: Rc<TypeConverter>,
        metric: String,
        datums: Vec<Datum>,
    ) -> Result<(), SinkError> {
        let mut try_again = true;
        while try_again {
            let connection = match connector.use_connection().await {
                Ok(connection) => connection,
                Err(error) => {
                    log::error!(
                        "Dropping metrics because I can't get a connection: {:?}",
                        error
                    );
                    continue;
                }
            };
            try_again =
                match PostgresSender::run_a_batch(&connection, &type_converter, &metric, &datums)
                    .await
                {
                    Ok(rows) => {
                        log::info!("committed rows: {rows}", rows = rows);

                        false
                    }
                    Err(e) => {
                        drop(connection);
                        let connection = connector.use_connection().await?;
                        match PostgresSender::handle_error_and_should_it_retry(&connection, e).await
                        {
                            Ok(should_retry) => should_retry,
                            Err(retry_failure) => {
                                log::error!("failed to handle error: {:?}", retry_failure);

                                false
                            }
                        }
                    }
                }
        }
        Ok(())
    }

    async fn run_a_batch(
        client: &PooledConnection<'_, PostgresConnectionManager<NoTls>>,
        type_converter: &TypeConverter,
        metric: &str,
        datums: &[Datum],
    ) -> Result<usize, SinkError> {
        let mut rows = 0;

        let dimension_types = type_converter.get_dimension_type_map(datums);
        let measurement_types = type_converter.get_measurement_type_map(datums);

        let all_column_types = get_all_column_types(&dimension_types, &measurement_types);
        let all_column_names = get_all_column_names(&dimension_types, &measurement_types);

        let sink: CopyInSink<bytes::Bytes> = match client
            .copy_in::<String, bytes::Bytes>(&format!(
                "copy {table_name} ({all_columns}) from stdin with binary",
                table_name = clean_id(metric),
                all_columns = all_column_names.join(","),
            ))
            .await
        {
            Ok(sink) => sink,
            Err(postgres_error) => match postgres_error.as_db_error() {
                Some(dberror) => match *dberror.code() {
                    SqlState::UNDEFINED_COLUMN => {
                        let pair = UNDEFINED_COLUMN.captures(dberror.message()).unwrap();
                        let table = pair.name("table").unwrap().as_str();
                        let column = pair.name("column").unwrap().as_str();
                        log::info!(
                            "missing column: {table}.{column}",
                            table = table,
                            column = column
                        );
                        let the_type = datums
                            .iter()
                            .filter_map(|d| match d.dimensions.get(column) {
                                Some(dim) => Some(sql_dimension_type_string(dim)),
                                None => match d.measurements.get(column) {
                                    Some(measurement) => Some(sql_data_type_string(measurement)),
                                    None => None,
                                },
                            })
                            .next();
                        match the_type {
                            Some(t) => {
                                return Err(SinkError::MissingColumn(MissingColumn {
                                    table: table.to_string(),
                                    column: column.to_string(),
                                    data_type: t.to_string(),
                                }))
                            }
                            None => {
                                return Err(SinkError::DescribedError(DescribedError {
                                    message: "Type not foud, can't add column".to_string(),
                                    inner: postgres_error,
                                }))
                            }
                        }
                    }
                    SqlState::UNDEFINED_TABLE => {
                        let table_capture = UNDEFINED_TABLE.captures(dberror.message()).unwrap();
                        let table = table_capture.name("table").unwrap().as_str();
                        log::info!("missing table: {table}", table = table);

                        return Err(SinkError::MissingTable(MissingTable {
                            table: table.to_string(),
                        }));
                    }
                    _ => {
                        return Err(SinkError::Postgres(postgres_error));
                    }
                },
                None => return Err(SinkError::Postgres(postgres_error)),
            },
        };

        let writer = BinaryCopyInWriter::new(sink, &all_column_types);
        rows += write_and_close(writer, &dimension_types, &measurement_types, datums).await?;

        Ok(rows)
    }

    async fn handle_error_and_should_it_retry(
        connection: &PooledConnection<'_, PostgresConnectionManager<NoTls>>,
        e: SinkError,
    ) -> Result<bool, SinkError> {
        return match e {
            SinkError::Postgres(postgres_error) => match postgres_error.as_db_error() {
                Some(dberror) => match *dberror.code() {
                    SqlState::INSUFFICIENT_PRIVILEGE => {
                        log::error!(
                            "Do you need to grant permissions or reset the table's owner? {:?}",
                            dberror
                        );

                        Ok(false)
                    }
                    _ => {
                        log::error!("unhandled db error: ${err:?}", err = dberror);

                        Ok(false)
                    }
                },
                None => match postgres_error.source() {
                    Some(client_error) => {
                        if client_error.is::<WrongType>() {
                            log::error!("Dropping batch due to mismatch between postgres type and batch type: {:?}", client_error);

                            Ok(false)
                        } else {
                            Ok(false)
                        }
                    }
                    None => {
                        log::error!("postgres without cause: ${err:?}", err = postgres_error);

                        Ok(false)
                    }
                },
            },
            SinkError::MissingColumn(what_column) => {
                log::info!("adding missing column {:?}", what_column);
                match connection.client().simple_query("select 1").await {
                    Ok(_) => {
                        log::info!("using connection for dml")
                    }
                    Err(e) => {
                        log::info!("connection is hosed: {:?}", e)
                    }
                }

                ddl::add_column(
                    connection.client(),
                    &what_column.table,
                    &what_column.column,
                    &what_column.data_type,
                )
                .await?;

                Ok(true)
            }
            SinkError::MissingTable(what_table) => {
                log::info!("adding missing table {:?}", what_table);
                ddl::create_table(connection.client(), &what_table.table).await?;

                Ok(true)
            }
            SinkError::DescribedError(_e) => todo!(),
            SinkError::StringError(_) => todo!(),
        };
    }
}

async fn write_and_close(
    writer: BinaryCopyInWriter,
    dimensions: &BTreeMap<String, Type>,
    measurements: &BTreeMap<String, Type>,
    data: &[Datum],
) -> Result<usize, SinkError> {
    pin_mut!(writer);
    log::debug!("writing {} rows", data.len());

    let mut row: Vec<Box<(dyn ToSql + Sync)>> = Vec::new();
    for datum in data {
        row.clear();
        let datum_time = SystemTime::UNIX_EPOCH + Duration::from_nanos(datum.unix_nanos);
        row.push(Box::new(datum_time));
        for dimension_name in dimensions.keys() {
            if !datum.dimensions.contains_key(dimension_name) {
                log::warn!("skipping dimension: {}", dimension_name);
                row.push(Box::new(Option::<String>::None));
                continue;
            }

            let dimension = &datum.dimensions[dimension_name];
            if let Some(value) = dimension.value.as_ref() {
                row.push(match value {
                    dimension::Value::String(s) => Box::new(s),
                    dimension::Value::Number(n) => Box::new(*n as i64),
                    dimension::Value::Boolean(b) => Box::new(b),
                })
            } else {
                row.push(Box::new(Option::<String>::None))
            }
        }
        for measurement_name in measurements.keys() {
            let measurement = &datum.measurements[measurement_name];
            if let Some(value) = measurement.value.as_ref() {
                row.push(match value {
                    measurement::Value::I64(i) => Box::new(i),
                    measurement::Value::I32(i) => Box::new(i),
                    measurement::Value::F64(f) => Box::new(f),
                    measurement::Value::F32(f) => Box::new(f),
                    // measurement::Value::StatisticSet(s) => Box::new((s.minimum, s.maximum, s.samplesum, s.samplecount)),
                    measurement::Value::StatisticSet(s) => Box::new(s),
                    measurement::Value::Histogram(h) => Box::new(h.to_stupidmap()),
                })
            } else {
                row.push(Box::new(Option::<f64>::None))
            }
        }

        let vec_of_raw_refs = row.iter().map(|c| c.as_ref()).collect_vec();
        writer.as_mut().write(&vec_of_raw_refs).await?;
    }
    writer.finish().await?;
    Ok(data.len())
}

// time, dimensions[], measurements[]
fn get_all_column_types(
    dimension_types: &BTreeMap<String, Type>,
    measurement_types: &BTreeMap<String, Type>,
) -> Vec<Type> {
    let mut all_column_types: Vec<Type> = vec![Type::TIMESTAMPTZ];
    all_column_types.extend(dimension_types.values().cloned());
    all_column_types.extend(measurement_types.values().cloned());
    all_column_types
}

// time, dimensions[], measurements[]
fn get_all_column_names(
    dimension_types: &BTreeMap<String, Type>,
    measurement_types: &BTreeMap<String, Type>,
) -> Vec<String> {
    let mut all_column_types: Vec<String> = vec!["time".to_string()];
    all_column_types.extend(dimension_types.keys().map(|d| clean_id(d)));
    all_column_types.extend(measurement_types.keys().map(|d| clean_id(d)));
    all_column_types
}

fn group_metrics(batch: Vec<Datum>) -> BTreeMap<String, Vec<Datum>> {
    let grouped_metrics: BTreeMap<String, Vec<Datum>> = batch
        .into_iter()
        // TODO: fix string copying here
        .sorted_by_key(|d| d.metric.clone())
        .group_by(|d| d.metric.clone())
        .into_iter()
        .map(|(metric, datums_iterable)| (metric, datums_iterable.collect::<Vec<Datum>>()))
        .collect();
    grouped_metrics
}

fn sql_data_type_string(measurement: &Measurement) -> &'static str {
    match measurement.value.as_ref().unwrap() {
        measurement::Value::I64(_) => "int8",
        measurement::Value::I32(_) => "int4",
        measurement::Value::F64(_) => "float8",
        measurement::Value::F32(_) => "float4",
        measurement::Value::StatisticSet(_) => "statistic_set",
        measurement::Value::Histogram(_) => "histogram",
    }
}

fn sql_dimension_type_string(dimension: &Dimension) -> &'static str {
    match dimension.value.as_ref().unwrap() {
        dimension::Value::String(_) => "text",
        dimension::Value::Number(_) => "int8",
        dimension::Value::Boolean(_) => "boolean",
    }
}
