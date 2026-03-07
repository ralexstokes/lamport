use crate::{mailbox::Mailbox, scheduler::SchedulerConfig};

pub(crate) fn normalize_scheduler_config(mut config: SchedulerConfig) -> SchedulerConfig {
    config.scheduler_count = config.scheduler_count.max(1);
    config.max_actors = config.max_actors.max(1);
    config.default_mailbox_capacity = config.default_mailbox_capacity.max(1);
    config.mailbox_runtime_reserve = config
        .mailbox_runtime_reserve
        .min(config.default_mailbox_capacity.saturating_sub(1));
    config.blocking_io_threads = config.blocking_io_threads.max(1);
    config.blocking_cpu_threads = config.blocking_cpu_threads.max(1);
    config.actor_turn_budget = config.actor_turn_budget.max(1);
    config
}

pub(crate) fn actor_mailbox(config: &SchedulerConfig, mailbox_capacity: Option<usize>) -> Mailbox {
    let mailbox_capacity = mailbox_capacity.unwrap_or(config.default_mailbox_capacity);
    Mailbox::with_limits(
        mailbox_capacity,
        config
            .mailbox_runtime_reserve
            .min(mailbox_capacity.saturating_sub(1)),
    )
}
