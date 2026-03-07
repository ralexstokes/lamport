use crate::types::ActorId;

/// Allocates actor ids as reusable slots plus generations.
#[derive(Debug, Default)]
pub(crate) struct ActorSlotAllocator {
    next_slot: u64,
    free_slots: Vec<u64>,
    generations: Vec<u64>,
}

impl ActorSlotAllocator {
    pub(crate) fn allocate(&mut self) -> ActorId {
        let slot = match self.free_slots.pop() {
            Some(slot) => slot,
            None => {
                let slot = self.next_slot;
                self.next_slot += 1;
                self.generations.push(0);
                slot
            }
        };

        let generation = self.generations[slot as usize];
        ActorId::new(slot, generation)
    }

    pub(crate) fn release(&mut self, actor: ActorId) {
        let Some(generation) = self.generations.get_mut(actor.local_id as usize) else {
            return;
        };

        if *generation != actor.generation {
            return;
        }

        *generation += 1;
        self.free_slots.push(actor.local_id);
    }
}
