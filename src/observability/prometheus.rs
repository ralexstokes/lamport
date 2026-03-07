use std::fmt::{self, Write};

use super::types::RuntimeMetricsSnapshot;

impl RuntimeMetricsSnapshot {
    /// Encodes the snapshot using Prometheus text exposition format.
    pub fn to_prometheus(&self) -> String {
        let mut output = String::new();
        let observed_at = self
            .observed_at
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        let scalars: &[(&str, &str, &str, &dyn fmt::Display)] = &[
            (
                "lamport_runtime_observed_at_seconds",
                "Unix timestamp when the runtime metrics snapshot was captured.",
                "gauge",
                &observed_at,
            ),
            (
                "lamport_runtime_live_actors",
                "Current number of live actors in the runtime.",
                "gauge",
                &self.live_actors,
            ),
            (
                "lamport_runtime_completed_actors",
                "Number of completed actor snapshots retained for introspection.",
                "gauge",
                &self.completed_actors,
            ),
            (
                "lamport_runtime_mailbox_messages",
                "Total number of messages queued across live actor mailboxes.",
                "gauge",
                &self.total_mailbox_len,
            ),
            (
                "lamport_runtime_mailbox_max_messages",
                "Largest mailbox length across live actors.",
                "gauge",
                &self.max_mailbox_len,
            ),
            (
                "lamport_scheduler_utilization_ratio",
                "Estimated fraction of busy scheduler time over the runtime lifetime.",
                "gauge",
                &self.scheduler_metrics.utilization,
            ),
            (
                "lamport_scheduler_normal_turns_total",
                "Total actor turns executed on normal schedulers.",
                "counter",
                &self.scheduler_metrics.normal_turns,
            ),
            (
                "lamport_scheduler_idle_turns_total",
                "Total idle polling turns observed by schedulers.",
                "counter",
                &self.scheduler_metrics.idle_turns,
            ),
            (
                "lamport_scheduler_blocking_io_jobs_total",
                "Total jobs submitted to the blocking I/O pool.",
                "counter",
                &self.scheduler_metrics.blocking_io_jobs,
            ),
            (
                "lamport_scheduler_blocking_cpu_jobs_total",
                "Total jobs submitted to the blocking CPU pool.",
                "counter",
                &self.scheduler_metrics.blocking_cpu_jobs,
            ),
        ];
        for (name, help, metric_type, value) in scalars {
            write_scalar_metric(&mut output, name, help, metric_type, value);
        }

        write_metric_header(
            &mut output,
            "lamport_runtime_actor_status",
            "Number of actors in each lifecycle state.",
            "gauge",
        );
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
                "lamport_runtime_actor_status",
                "status",
                status,
                value,
            );
        }

        write_metric_header(
            &mut output,
            "lamport_scheduler_runnable_actors",
            "Number of runnable actors assigned to each scheduler.",
            "gauge",
        );
        write_metric_header(
            &mut output,
            "lamport_scheduler_waiting_actors",
            "Number of waiting actors assigned to each scheduler.",
            "gauge",
        );
        write_metric_header(
            &mut output,
            "lamport_scheduler_injected_total",
            "Actors injected onto each scheduler from the global queue.",
            "counter",
        );
        write_metric_header(
            &mut output,
            "lamport_scheduler_stolen_total",
            "Actors stolen by each scheduler.",
            "counter",
        );

        let mut run_queues = self.run_queues.clone();
        run_queues.sort_by_key(|snapshot| snapshot.scheduler_id);

        for snapshot in run_queues {
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_runnable_actors",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.runnable,
            );
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_waiting_actors",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.waiting,
            );
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_injected_total",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.injected,
            );
            write_single_label_metric(
                &mut output,
                "lamport_scheduler_stolen_total",
                "scheduler_id",
                snapshot.scheduler_id,
                snapshot.stolen,
            );
        }

        output
    }
}

fn write_metric_header(output: &mut String, name: &str, help: &str, metric_type: &str) {
    writeln!(output, "# HELP {name} {help}").unwrap();
    writeln!(output, "# TYPE {name} {metric_type}").unwrap();
}

fn write_scalar_metric(
    output: &mut String,
    name: &str,
    help: &str,
    metric_type: &str,
    value: impl fmt::Display,
) {
    write_metric_header(output, name, help, metric_type);
    writeln!(output, "{name} {value}").unwrap();
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
