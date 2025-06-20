/*
 *     Copyright 2023 The Dragonfly Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use dragonfly_client_config::dfdaemon::Host;
use opentelemetry::{global, trace::TracerProvider, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{propagation::TraceContextPropagator, Resource};
use rolling_file::*;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{info, Level};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{time::ChronoLocal, Layer},
    prelude::*,
    EnvFilter, Registry,
};

/// SPAN_EXPORTER_TIMEOUT is the timeout for the span exporter.
const SPAN_EXPORTER_TIMEOUT: Duration = Duration::from_secs(10);

/// init_tracing initializes the tracing system.
#[allow(clippy::too_many_arguments)]
pub fn init_tracing(
    name: &str,
    log_dir: PathBuf,
    log_level: Level,
    log_max_files: usize,
    jaeger_addr: Option<String>,
    host: Option<Host>,
    is_seed_peer: bool,
    console: bool,
) -> Vec<WorkerGuard> {
    let mut guards = vec![];

    // Setup stdout layer.
    let (stdout_writer, stdout_guard) = tracing_appender::non_blocking(std::io::stdout());
    guards.push(stdout_guard);

    // Initialize stdout layer.
    let stdout_filter = if console {
        LevelFilter::DEBUG
    } else {
        LevelFilter::OFF
    };
    let stdout_logging_layer = Layer::new()
        .with_writer(stdout_writer)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .with_thread_names(false)
        .with_thread_ids(false)
        .with_timer(ChronoLocal::rfc_3339())
        .pretty()
        .with_filter(stdout_filter);

    // Setup file layer.
    fs::create_dir_all(log_dir.clone()).expect("failed to create log directory");
    let rolling_appender = BasicRollingFileAppender::new(
        log_dir.join(name).with_extension("log"),
        RollingConditionBasic::new().hourly(),
        log_max_files,
    )
    .expect("failed to create rolling file appender");

    let (rolling_writer, rolling_writer_guard) = tracing_appender::non_blocking(rolling_appender);
    guards.push(rolling_writer_guard);

    let file_logging_layer = Layer::new()
        .with_writer(rolling_writer)
        .with_ansi(false)
        .with_file(true)
        .with_line_number(true)
        .with_target(false)
        .with_thread_names(false)
        .with_thread_ids(false)
        .with_timer(ChronoLocal::rfc_3339())
        .compact();

    // Setup env filter for log level.
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::default().add_directive(log_level.into()));

    let subscriber = Registry::default()
        .with(env_filter)
        .with(file_logging_layer)
        .with(stdout_logging_layer);

    // Setup jaeger layer.
    if let Some(mut jaeger_addr) = jaeger_addr {
        jaeger_addr = if jaeger_addr.starts_with("http://") {
            jaeger_addr
        } else {
            format!("http://{}", jaeger_addr)
        };

        let otlp_exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(jaeger_addr)
            .with_timeout(SPAN_EXPORTER_TIMEOUT)
            .build()
            .expect("failed to create OTLP exporter");

        let host = host.unwrap();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(otlp_exporter)
            .with_resource(
                Resource::builder()
                    .with_service_name(format!("{}-{}", name, host.ip.unwrap()))
                    .with_schema_url(
                        [
                            KeyValue::new(
                                opentelemetry_semantic_conventions::attribute::SERVICE_NAMESPACE,
                                "dragonfly",
                            ),
                            KeyValue::new(
                                opentelemetry_semantic_conventions::attribute::HOST_NAME,
                                host.hostname,
                            ),
                            KeyValue::new(
                                opentelemetry_semantic_conventions::attribute::HOST_IP,
                                host.ip.unwrap().to_string(),
                            ),
                        ],
                        opentelemetry_semantic_conventions::SCHEMA_URL,
                    )
                    .with_attribute(opentelemetry::KeyValue::new(
                        "host.idc",
                        host.idc.unwrap_or_default(),
                    ))
                    .with_attribute(opentelemetry::KeyValue::new(
                        "host.location",
                        host.location.unwrap_or_default(),
                    ))
                    .with_attribute(opentelemetry::KeyValue::new("host.seed_peer", is_seed_peer))
                    .build(),
            )
            .build();

        let tracer = provider.tracer(name.to_string());
        global::set_tracer_provider(provider.clone());
        global::set_text_map_propagator(TraceContextPropagator::new());

        let jaeger_layer = OpenTelemetryLayer::new(tracer);
        subscriber.with(jaeger_layer).init();
    } else {
        subscriber.init();
    }

    info!(
        "tracing initialized directory: {}, level: {}",
        log_dir.as_path().display(),
        log_level
    );

    guards
}
