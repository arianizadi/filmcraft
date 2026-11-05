use super::*;
use crate::{ClipId, SequenceSettings, TrackKind};
use filmcraft_time::{Tick, TimeRange};
fn project() -> (Project, ItemId, ClipId) {
    let mut p = Project::new("test");
    let source = p.add_item("source", crate::label::Label::Iris, ItemKind::Graphic { width: 16, height: 16, rate: filmcraft_time::FrameRate::FPS_24 }, None);
    let q = p.new_sequence("seq", SequenceSettings::default(), 1, 0, None);
    let item =
        p.make_track_item(source, TrackKind::Video, Tick::ZERO, TimeRange::new(Tick::ZERO, Tick::from_units(1, 1)), filmcraft_time::FrameRate::FPS_24).unwrap();
    let clip = item.id;
    p.sequence_mut(q).unwrap().video_tracks[0].items.push(item);
    (p, q, clip)
}
#[test]
fn deterministic_missing_ids_and_strict_duplicate_validation() {
    let (mut p, q, clip) = project();
    for e in &mut p.sequence_mut(q).unwrap().find_item_mut(clip).unwrap().1.effects {
        e.instance_id = 0;
    }
    p.next_effect_instance_id = 1;
    let mut other = p.clone();
    p.ensure_effect_instance_ids().unwrap();
    other.ensure_effect_instance_ids().unwrap();
    let identities = |p: &Project| clips(p).flat_map(|(_, clip)| clip.effects.iter().map(|e| e.instance_id)).collect::<Vec<_>>();
    assert_eq!(identities(&p), identities(&other));
    assert_eq!(p.next_effect_instance_id, other.next_effect_instance_id);
    let effects = &p.sequence(q).unwrap().find_item(clip).unwrap().1.effects;
    assert!(effects.iter().all(|e| e.instance_id > 0));
    let id = effects[0].instance_id;
    assert_eq!(find_instance(effects, &format!("effect:{id}")).unwrap(), 0);
    p.sequence_mut(q).unwrap().find_item_mut(clip).unwrap().1.effects[1].instance_id = id;
    assert!(p.ensure_effect_instance_ids().is_err());
}
#[test]
fn changed_sequence_copies_get_fresh_ids_and_reordering_preserves_originals() {
    let (mut p, q, clip) = project();
    let before = p.clone();
    let ids: Vec<_> = p.sequence(q).unwrap().find_item(clip).unwrap().1.effects.iter().map(|e| e.instance_id).collect();
    p.sequence_mut(q).unwrap().find_item_mut(clip).unwrap().1.effects.reverse();
    p.assign_changed_effect_ids(&before, 1).unwrap();
    let reordered: Vec<_> = p.sequence(q).unwrap().find_item(clip).unwrap().1.effects.iter().map(|e| e.instance_id).collect();
    assert_eq!(reordered, ids.iter().copied().rev().collect::<Vec<_>>());
    let before = p.clone();
    let copy = ItemId(p.alloc_id());
    let mut item = p.item(q).unwrap().clone();
    item.id = copy;
    p.items.insert(copy, item);
    p.assign_changed_effect_ids(&before, 1).unwrap();
    let cloned: Vec<_> = p.sequence(copy).unwrap().find_item(clip).unwrap().1.effects.iter().map(|e| e.instance_id).collect();
    assert!(cloned.iter().all(|id| !ids.contains(id)));
    let originals: Vec<_> = p.sequence(q).unwrap().find_item(clip).unwrap().1.effects.iter().map(|e| e.instance_id).collect();
    assert_eq!(originals, reordered);
}
#[test]
fn hostile_counter_and_selector_never_panic() {
    let (mut p, q, clip) = project();
    for next in [0, u64::MAX] {
        p.next_effect_instance_id = next;
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.ensure_effect_instance_ids())).unwrap().is_err());
    }
    for selector in ["effect:0", "effect:18446744073709551615", "effect:abc", "lumetri:99999999999999999999", "bad"] {
        assert!(find_instance(&p.sequence(q).unwrap().find_item(clip).unwrap().1.effects, selector).is_err());
    }
}
#[test]
fn move_from_removed_sequence_preserves_ids_and_exhaustion_does_not_partially_migrate() {
    let (mut p, old, clip) = project();
    let before = p.clone();
    let item = p.sequence(old).unwrap().find_item(clip).unwrap().1.clone();
    let ids: Vec<_> = item.effects.iter().map(|e| e.instance_id).collect();
    let dest = p.new_sequence("destination", SequenceSettings::default(), 1, 0, None);
    p.items.remove(&old);
    p.sequence_mut(dest).unwrap().video_tracks[0].items.push(item);
    p.assign_changed_effect_ids(&before, 1).unwrap();
    assert_eq!(p.sequence(dest).unwrap().find_item(clip).unwrap().1.effects.iter().map(|e| e.instance_id).collect::<Vec<_>>(), ids);
    for e in &mut p.sequence_mut(dest).unwrap().find_item_mut(clip).unwrap().1.effects {
        e.instance_id = 0;
    }
    p.next_effect_instance_id = MAX_EFFECT_INSTANCE_ID;
    assert!(p.ensure_effect_instance_ids().is_err());
    assert!(p.sequence(dest).unwrap().find_item(clip).unwrap().1.effects.iter().all(|e| e.instance_id == 0));
}
