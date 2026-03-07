use super::{EventCursor, RuntimeEvent};

pub(crate) fn events_since(events: &[RuntimeEvent], cursor: &mut EventCursor) -> Vec<RuntimeEvent> {
    let start = events.partition_point(|event| event.sequence < cursor.next_sequence);
    let pending = events[start..].to_vec();
    if let Some(last) = pending.last() {
        cursor.next_sequence = last.sequence + 1;
    }
    pending
}
