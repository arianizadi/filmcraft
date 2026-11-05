# Stable grade addresses

`grade.get`, `grade.patch`, `grade.copy`, `grade.reset`, and `grade.matchBatch` accept
`effectInstance: "effect:123"`. Read the address from `grade.get` or a successful creation
response. Responses include the numeric `effectInstanceId` and the compatibility
`legacyEffectInstance: "lumetri:0"`. Sequence inspection also includes the stable address.

The identity belongs to a placed clip effect and survives reordered or removed neighboring
effects, parameter resets, a grade copied onto that target, undo/redo, and project reopening.
Duplicated clips, sequences, and newly added effects receive fresh identities. Reusable gallery,
look, and preset definitions carry no placed identity. They preserve the target's corresponding
same-kind effect identity when applied; additional effects receive fresh identities.

Existing `lumetri:<ordinal>` addresses remain accepted. Every grade write, including a write
using a stable address, requires `expectedRevision`; a stale revision refuses the write before
mutation. Plan steps receive their sandbox revision automatically. A later step may refer to a
new instance with `{"$step": 0, "path": "/effectInstance"}`. Applying that plan preserves the
previewed address, while normal plan revision checks still apply.

Legacy projects assign missing placed identities deterministically on open. Persisted duplicate
nonzero identities, values outside JSON's exact integer range, and exhausted counters return
an error. The project stores a separate monotonic effect counter; the live session retains its
high-water mark across undo, and saving records it so a deleted address cannot silently refer
to a future effect after reopening. These IDs do not change existing clip, item, or track IDs.
