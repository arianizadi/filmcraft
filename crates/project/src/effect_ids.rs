//! Persistent identity of placed clip effects. Definition/template values may retain zero IDs;
//! only instances on timeline clips share the project's monotonic identity namespace.
use crate::{EffectInstance, ItemId, ItemKind, Project, TrackItem};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
/// JSON-exact integers, also represented as `effect:<id>` in agent addresses.
pub const MAX_EFFECT_INSTANCE_ID: u64 = (1u64 << 53) - 1;
const MAX_EFFECTS: usize = 1_000_000;
pub fn initial_counter() -> u64 {
    1
}
fn valid_counter(next: u64) -> Result<(), String> {
    if next == 0 || next > MAX_EFFECT_INSTANCE_ID + 1 { Err("effect identity counter is invalid or exhausted".into()) } else { Ok(()) }
}
fn take(next: &mut u64) -> Result<u64, String> {
    if *next == 0 || *next > MAX_EFFECT_INSTANCE_ID {
        return Err("effect identity space exhausted".into());
    }
    let id = *next;
    *next = next.checked_add(1).ok_or("effect identity space exhausted")?;
    Ok(id)
}
/// Stable addresses never depend on effect ordering; legacy name/ordinal selectors remain accepted.
pub fn find_instance(effects: &[EffectInstance], selector: &str) -> Result<usize, String> {
    if selector.len() > 128 {
        return Err("effect selector exceeds 128 bytes".into());
    }
    if let Some(id) = selector.strip_prefix("effect:") {
        let id = id.parse::<u64>().ok().filter(|id| *id > 0 && *id <= MAX_EFFECT_INSTANCE_ID).ok_or("invalid stable effect identity")?;
        let mut matches = effects.iter().enumerate().filter(|(_, e)| e.instance_id == id);
        let index = matches.next().map(|p| p.0).ok_or("effect identity does not belong to this clip")?;
        if matches.next().is_some() {
            return Err("duplicate effect identity on clip".into());
        }
        return Ok(index);
    }
    let (name, ordinal) = selector.rsplit_once(':').ok_or("effect selector must be effect:<id> or name:<ordinal>")?;
    let ordinal = ordinal.parse::<usize>().ok().filter(|n| *n <= MAX_EFFECTS).ok_or("invalid effect ordinal")?;
    effects.iter().enumerate().filter(|(_, e)| e.effect == name).nth(ordinal).map(|p| p.0).ok_or_else(|| "no such effect ordinal".into())
}
fn clips(p: &Project) -> impl Iterator<Item = (ItemId, &TrackItem)> {
    p.items
        .iter()
        .flat_map(|(id, item)| item.as_sequence().into_iter().flat_map(move |q| q.all_tracks().flat_map(move |t| t.items.iter().map(move |it| (*id, it)))))
}
fn same_layout(a: &crate::Sequence, b: &crate::Sequence, counter: u64) -> bool {
    let mut left = a.all_tracks().flat_map(|t| &t.items);
    let mut right = b.all_tracks().flat_map(|t| &t.items);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(a), Some(b))
                if a.id == b.id
                    && a.effects.len() == b.effects.len()
                    && a.effects.iter().zip(&b.effects).all(|(a, b)| a.instance_id != 0 && a.instance_id < counter && a.instance_id == b.instance_id) => {}
            _ => return false,
        }
    }
}

impl Project {
    /// Load-time deterministic migration. Duplicate nonzero identities are corrupt, never guessed.
    pub fn ensure_effect_instance_ids(&mut self) -> Result<(), String> {
        valid_counter(self.next_effect_instance_id)?;
        let mut seen = BTreeSet::new();
        let mut count = 0usize;
        let mut largest = 0u64;
        let mut missing = 0u64;
        for (_, it) in clips(self) {
            for effect in &it.effects {
                count = count.saturating_add(1);
                if count > MAX_EFFECTS {
                    return Err("project exceeds 1000000 placed effects".into());
                }
                let id = effect.instance_id;
                if id > MAX_EFFECT_INSTANCE_ID {
                    return Err("effect identity exceeds JSON-exact range".into());
                }
                if id != 0 && !seen.insert(id) {
                    return Err("duplicate persisted effect instance identity".into());
                }
                largest = largest.max(id);
                missing = missing.saturating_add(u64::from(id == 0));
            }
        }
        let mut next = self.next_effect_instance_id.max(largest.saturating_add(1));
        valid_counter(next)?;
        if next.checked_add(missing).is_none_or(|end| end > MAX_EFFECT_INSTANCE_ID + 1) {
            return Err("effect identity space exhausted".into());
        }
        for item in self.items.values_mut() {
            let ItemKind::Sequence(q) = &mut item.kind else {
                continue;
            };
            if !q.all_tracks().flat_map(|t| &t.items).flat_map(|it| &it.effects).any(|e| e.instance_id == 0) {
                continue;
            }
            for it in Arc::make_mut(q).all_tracks_mut().flat_map(|t| &mut t.items) {
                for effect in &mut it.effects {
                    if effect.instance_id == 0 {
                        effect.instance_id = take(&mut next)?;
                    }
                }
            }
        }
        self.next_effect_instance_id = next;
        Ok(())
    }
    /// Assign freshly constructed effects without changing the Item/Clip identity allocator.
    pub fn fresh_effect_instance_ids(&mut self, effects: &mut [EffectInstance]) -> Result<(), String> {
        valid_counter(self.next_effect_instance_id)?;
        if effects.len() > MAX_EFFECTS {
            return Err("too many effects".into());
        }
        let mut next = self.next_effect_instance_id;
        if u64::try_from(effects.len()).ok().and_then(|n| next.checked_add(n)).is_none_or(|end| end > MAX_EFFECT_INSTANCE_ID + 1) {
            return Err("effect identity space exhausted".into());
        }
        for effect in effects {
            effect.instance_id = take(&mut next)?;
        }
        self.next_effect_instance_id = next;
        Ok(())
    }
    /// Commit-time repair visits only COW-changed sequences. Existing owners retain IDs, copies get
    /// fresh IDs, and already-allocated staged IDs above the prior counter survive plan application.
    pub fn assign_changed_effect_ids(&mut self, before: &Project, floor: u64) -> Result<(), String> {
        valid_counter(self.next_effect_instance_id)?;
        valid_counter(before.next_effect_instance_id)?;
        valid_counter(floor.max(1))?;
        let boundary = before.next_effect_instance_id.max(floor).max(1);
        let allocated_end = self.next_effect_instance_id;
        let mut next = allocated_end.max(boundary);
        let changed: Vec<_> = self
            .items
            .iter()
            .filter_map(|(id, item)| {
                let ItemKind::Sequence(q) = &item.kind else {
                    return None;
                };
                if matches!(before.items.get(id).map(|i|&i.kind),Some(ItemKind::Sequence(old))if Arc::ptr_eq(q,old)||same_layout(q,old,before.next_effect_instance_id)) { None } else { Some(*id) }
            })
            .collect();
        let mut old_sequences = changed.clone();
        old_sequences.extend(before.sequences().filter(|item| self.sequence(item.id).is_none()).map(|item| item.id));
        let mut old_owners = BTreeMap::new();
        let mut present = BTreeSet::new();
        let mut count = 0usize;
        for id in &old_sequences {
            if let Some(q) = before.sequence(*id) {
                for it in q.all_tracks().flat_map(|t| &t.items) {
                    for e in &it.effects {
                        count = count.saturating_add(1);
                        if count > MAX_EFFECTS {
                            return Err("changed effects exceed 1000000 instances".into());
                        }
                        if e.instance_id != 0 {
                            if e.instance_id > MAX_EFFECT_INSTANCE_ID || old_owners.insert(e.instance_id, (*id, it.id)).is_some() {
                                return Err("invalid or duplicate prior effect instance identity".into());
                            }
                            next = next.max(e.instance_id.saturating_add(1));
                        }
                    }
                }
            }
            if let Some(q) = self.sequence(*id) {
                for it in q.all_tracks().flat_map(|t| &t.items) {
                    if !present.insert((*id, it.id)) {
                        return Err("duplicate clip identity in changed sequence".into());
                    }
                }
            }
        }
        let mut accepted = BTreeSet::new();
        let mut count = 0usize;
        for id in changed {
            let q = self.sequence_mut(id).ok_or("changed sequence vanished")?;
            for it in q.all_tracks_mut().flat_map(|t| &mut t.items) {
                for e in &mut it.effects {
                    count = count.saturating_add(1);
                    if count > MAX_EFFECTS {
                        return Err("changed effects exceed 1000000 instances".into());
                    }
                    let old = old_owners.get(&e.instance_id).copied();
                    let original = old.is_some_and(|owner| owner == (id, it.id) || owner.1 == it.id && !present.contains(&owner));
                    let staged = e.instance_id >= boundary && e.instance_id < allocated_end;
                    if e.instance_id != 0 && e.instance_id <= MAX_EFFECT_INSTANCE_ID && (original || staged) && accepted.insert(e.instance_id) {
                        continue;
                    }
                    e.instance_id = take(&mut next)?;
                    accepted.insert(e.instance_id);
                }
            }
        }
        self.next_effect_instance_id = next;
        Ok(())
    }
}
#[cfg(test)]
mod tests;
