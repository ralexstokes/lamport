use std::fmt::{self, Write};

use super::types::RuntimeMetricsSnapshot;

#[derive(Clone, Copy)]
struct MetricFamily {
    name: &'static str,
    help: &'static str,
    metric_type: &'static str,
}

struct ScalarMetric<'a> {
    family: MetricFamily,
    value: &'a dyn fmt::Display,
}

impl RuntimeMetricsSnapshot {
    /// Encodes the snapshot using Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut output = String::new();
        let observed_at = self
            .observed_at
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let scalar_metrics = [
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_runtime_observed_at_seconds",
                    help: "Unix timestamp when the runtime metrics snapshot was captured.",
                    metric_type: "gauge",
                },
                value: &observed_at,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_runtime_live_actors",
                    help: "Current number of live actors in the runtime.",
                    metric_type: "gauge",
                },
                value: &self.live_actors,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_runtime_completed_actors",
                    help: "Number of completed actor snapshots retained for introspection.",
                    metric_type: "gauge",
                },
                value: &self.completed_actors,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_runtime_mailbox_messages",
                    help: "Total number of messages queued across live actor mailboxes.",
                    metric_type: "gauge",
                },
                value: &self.total_mailbox_len,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_runtime_mailbox_max_messages",
                    help: "Largest mailbox length across live actors.",
                    metric_type: "gauge",
                },
                value: &self.max_mailbox_len,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_scheduler_utilization_ratio",
                    help: "Estimated fraction of busy scheduler time over the runtime lifetime.",
                    metric_type: "gauge",
                },
                value: &self.scheduler_metrics.utilization,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_scheduler_normal_turns_total",
                    help: "Total actor turns executed on normal schedulers.",
                    metric_type: "counter",
                },
                value: &self.scheduler_metrics.normal_turns,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_scheduler_idle_turns_total",
                    help: "Total idle polling turns observed by schedulers.",
                    metric_type: "counter",
                },
                value: &self.scheduler_metrics.idle_turns,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_scheduler_blocking_io_jobs_total",
                    help: "Total jobs submitted to the blocking I/O pool.",
                    metric_type: "counter",
                },
                value: &self.scheduler_metrics.blocking_io_jobs,
            },
            ScalarMetric {
                family: MetricFamily {
                    name: "lamport_scheduler_blocking_cpu_jobs_total",
                    help: "Total jobs submitted to the blocking CPU pool.",
                    metric_type: "counter",
                },
                value: &self.scheduler_metrics.blocking_cpu_jobs,
            },
        ];
        for metric in scalar_metrics {
            write_scalar_metric(&mut output, metric);
        }

        let actor_status_family = MetricFamily {
            name: "lamport_runtime_actor_status",
            help: "Number of actors in each lifecycle state.",
            metric_type: "gauge",
        };
        write_metric_header(&mut output, actor_status_family);
        for (status, value) in [
            ("starting", self.starting_actors),
            ("runnable", self.runnable_actors),
            ("waiting", self.waiting_actors),
            ("running", self.running_actors),
            ("exiting", self.exiting_actors),
            ("dead", self.dead_actors),
        ] {
            write_single_label_metric(
                &mut output,
                actor_status_family.name,
                "status",
                status,
                value,
            );
        }

        let scheduler_runnable_family = MetricFamily {
            name: "lamport_scheduler_runnable_actors",
            help: "Number of runnable actors assigned to each scheduler.",
            metric_type: "gauge",
        };
        let scheduler_waiting_family = MetricFamily {
            name: "lamport_scheduler_waiting_actors",
            help: "Number of waiting actors assigned to each scheduler.",
            metric_type: "gauge",
        };
        let scheduler_injected_family = MetricFamily {
            name: "lamport_scheduler_injected_total",
            help: "Actors injected onto each scheduler from the global queue.",
            metric_type: "counter",
        };
        let scheduler_stolen_family = MetricFamily {
            name: "lamport_scheduler_stolen_total",
            help: "Actors stolen by each scheduler.",
            metric_type: "counter",
        };
        for family in [
            scheduler_runnable_family,
            scheduler_waiting_family,
            scheduler_injected_family,
            scheduler_stolen_family,
        ] {
            write_metric_header(&mut output, family);
        }

        let mut run_queues: Vec<_> = self.run_queues.iter().collect();
        run_queues.sort_by_key(|snapshot| snapshot.scheduler_id);

        for snapshot in run_queues {
            write_single_label_metric(
                &mut output,
                scheduler_runnable_family.name,
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.runnable,
            );
            write_single_label_metric(
                &mut output,
                scheduler_waiting_family.name,
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.waiting,
            );
            write_single_label_metric(
                &mut output,
                scheduler_injected_family.name,
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.injected,
            );
            write_single_label_metric(
                &mut output,
                scheduler_stolen_family.name,
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.stolen,
            );
        }

        output
    }
}

fn write_metric_header(output: &mut String, family: MetricFamily) {
    writeln!(
        output,
        "# HELP {name} {help}",
        name = family.name,
        help = family.help
    )
    .unwrap();
    writeln!(
        output,
        "# TYPE {name} {metric_type}",
        name = family.name,
        metric_type = family.metric_type
    )
    .unwrap();
}

fn write_scalar_metric(output: &mut String, metric: ScalarMetric<'_>) {
    write_metric_header(output, metric.family);
    writeln!(
        output,
        "{name} {value}",
        name = metric.family.name,
        value = metric.value
    )
    .unwrap();
}

fn write_single_label_metric(
    output: &mut String,
    name: &str,
    label_name: &str,
    label_value: impl fmt::Display,
    value: impl fmt::Display,
) {
    writeln!(output, "{name}{{{label_name}=\"{label_value}\"}} {value}").unwrap();
}
