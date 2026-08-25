-- Operator-chosen names for catalog models.
--
-- A model id is an address. `xai/grok-4.6` is what a client sends, what a rung
-- names, and what the ledger records spend under, so renaming one silently
-- rewrites a client's config, a route's ladder and the join key on every
-- historical row at once. A label is a name, and renaming a name should cost
-- nothing. Until now the catalog had only the address, which is exactly why
-- renaming anything felt dangerous: there was nowhere to put a name.
--
-- Nullable rather than backfilled with today's derived form. NULL means "nobody
-- has named this one", so it keeps following the provider's own spelling as the
-- catalog is refreshed; a backfill would freeze one afternoon's derivation into
-- every row and make an improvement to the derivation invisible. It also makes
-- "clear the label" expressible, which is what restores the default.
--
-- Expand-only. The previous release neither reads nor writes this column, so it
-- runs unchanged against the new schema and a rollback loses labels and nothing
-- else.

ALTER TABLE model_catalog
    -- What a picker shows. NULL = derive it from the provider's display name
    -- and the upstream model name.
    --
    -- Deliberately absent from `upsert_model`'s ON CONFLICT DO UPDATE list, for
    -- the same reason `is_override` is absent from it: a LiteLLM seed and a
    -- provider price sync know nothing about what an operator decided to call a
    -- model, so a refresh that carried this column would undo every rename on
    -- its next tick. The write path for it is one endpoint of its own.
    ADD COLUMN display_label text;
