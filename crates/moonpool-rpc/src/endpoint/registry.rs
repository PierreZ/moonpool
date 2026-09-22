//! The dynamic endpoint registry: an indexed slot vector with a free list
//! and checked, non-wrapping generations (FDB `EndpointMap`, without its bit
//! layout).

use super::EndpointToken;

/// Why a registration was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegistryError {
    /// The configured endpoint budget is exhausted.
    Full,
}

#[derive(Debug)]
struct Slot<T> {
    generation: u32,
    value: Option<T>,
}

/// Slots indexed by token index; a token is valid only while its slot holds
/// a value registered under the token's generation.
#[derive(Debug)]
pub(crate) struct Registry<T> {
    slots: Vec<Slot<T>>,
    /// Reusable slot indices, most recently freed last.
    free: Vec<u64>,
    live: usize,
    max_live: usize,
}

impl<T> Registry<T> {
    pub(crate) fn new(max_live: usize) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            live: 0,
            max_live,
        }
    }

    pub(crate) fn live(&self) -> usize {
        self.live
    }

    pub(crate) fn insert(&mut self, value: T) -> Result<EndpointToken, RegistryError> {
        if self.live >= self.max_live {
            return Err(RegistryError::Full);
        }
        let index = if let Some(index) = self.free.pop() {
            index
        } else {
            let index = u64::try_from(self.slots.len()).map_err(|_| RegistryError::Full)?;
            self.slots.push(Slot {
                generation: 0,
                value: None,
            });
            index
        };
        let slot = Self::slot_mut(&mut self.slots, index).ok_or(RegistryError::Full)?;
        assert!(slot.value.is_none(), "free-listed slot must be empty");
        slot.value = Some(value);
        self.live += 1;
        Ok(EndpointToken::from_parts(index, slot.generation))
    }

    fn slot_mut(slots: &mut [Slot<T>], index: u64) -> Option<&mut Slot<T>> {
        slots.get_mut(usize::try_from(index).ok()?)
    }

    pub(crate) fn get(&self, token: EndpointToken) -> Option<&T> {
        let slot = self.slots.get(usize::try_from(token.index()).ok()?)?;
        if slot.generation == token.generation() {
            slot.value.as_ref()
        } else {
            None
        }
    }

    /// Remove the value registered under `token`; a stale token removes
    /// nothing. The slot's generation advances so the token never matches
    /// again; a slot whose generation cannot advance is retired, never
    /// wrapped.
    pub(crate) fn remove(&mut self, token: EndpointToken) -> Option<T> {
        let slot = Self::slot_mut(&mut self.slots, token.index())?;
        if slot.generation != token.generation() || slot.value.is_none() {
            return None;
        }
        let value = slot.value.take();
        self.live -= 1;
        // An exhausted slot is retired: it simply never returns to the free
        // list.
        if let Some(next) = slot.generation.checked_add(1) {
            slot.generation = next;
            self.free.push(token.index());
        }
        value
    }

    #[cfg(test)]
    fn force_generation(&mut self, index: u64, generation: u32) {
        if let Some(slot) = Self::slot_mut(&mut self.slots, index) {
            slot.generation = generation;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Registry, RegistryError};
    use crate::endpoint::EndpointToken;

    #[test]
    fn reuse_advances_the_generation_and_rejects_the_old_token() {
        let mut registry = Registry::new(8);
        let first = registry.insert("a").expect("room");
        assert_eq!(registry.get(first), Some(&"a"));
        assert_eq!(registry.remove(first), Some("a"));
        assert_eq!(registry.get(first), None);

        let second = registry.insert("b").expect("room");
        assert_eq!(second.index(), first.index(), "slot is reused");
        assert_ne!(second.generation(), first.generation());
        assert_eq!(registry.get(first), None, "old token never aliases");
        assert_eq!(registry.remove(first), None, "stale remove is a no-op");
        assert_eq!(registry.get(second), Some(&"b"));
    }

    #[test]
    fn repeated_allocate_drop_keeps_one_slot_and_live_count() {
        let mut registry = Registry::new(2);
        let mut previous = Vec::new();
        for round in 0..100u32 {
            let token = registry.insert(round).expect("room");
            assert_eq!(registry.live(), 1);
            for old in &previous {
                assert_eq!(registry.get(*old), None);
            }
            assert_eq!(registry.remove(token), Some(round));
            previous.push(token);
        }
        assert_eq!(registry.live(), 0);
        assert!(previous.iter().all(|token| token.index() == 0));
    }

    #[test]
    fn exhausted_generation_retires_the_slot_instead_of_wrapping() {
        let mut registry = Registry::new(4);
        let token = registry.insert(1).expect("room");
        registry.force_generation(token.index(), u32::MAX);
        let token = EndpointToken::from_parts(token.index(), u32::MAX);
        assert_eq!(registry.remove(token), Some(1));
        assert!(registry.free.is_empty(), "retired slot is not free-listed");
        let fresh = registry.insert(2).expect("room");
        assert_ne!(fresh.index(), token.index(), "retired slot is not reused");
        assert_eq!(
            registry.get(EndpointToken::from_parts(token.index(), 0)),
            None
        );
    }

    #[test]
    fn budget_is_enforced_and_released() {
        let mut registry = Registry::new(1);
        let token = registry.insert(()).expect("room");
        assert_eq!(registry.insert(()), Err(RegistryError::Full));
        registry.remove(token);
        assert!(registry.insert(()).is_ok());
    }

    #[test]
    fn out_of_range_tokens_are_not_found() {
        let registry: Registry<()> = Registry::new(1);
        assert_eq!(registry.get(EndpointToken::from_parts(99, 0)), None);
    }
}
