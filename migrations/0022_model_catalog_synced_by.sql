-- Which endpoint's catalog sync wrote a model row.
--
-- `oag admin endpoint sync` removes the rows of an endpoint that its list no
-- longer offers. Until now it told its own rows from everyone else's by the
-- provider alone, so it also removed rows it never wrote: a model an operator
-- added by hand with `oag admin catalog add`, such as the free one the sync
-- itself tells them to add that way, and a model the list still names but
-- the sync skipped. Each such row was deleted by the next sync.
--
-- `synced_by` is the endpoint whose sync last wrote the row, and NULL for
-- every row a sync did not write. A sync sets it on each row it inserts or
-- rewrites, and removes only rows whose `synced_by` is its own endpoint.
-- `oag admin catalog add` clears it: a row the operator stated is theirs,
-- and no sync removes it, whatever the list says.
--
-- Rows that exist when this runs get NULL, the rows earlier syncs wrote
-- among them, so no sync removes those until it has written them again. The
-- direction of the error is deliberate: a stale row an operator can delete
-- costs less than a row someone wanted that a sync deleted.
--
-- Expand-only. The previous release names its columns in every statement, so
-- it neither reads this column nor writes it: a row it inserts gets NULL, and
-- a row it rewrites keeps whatever this column held.

ALTER TABLE model_catalog ADD COLUMN synced_by text;

COMMENT ON COLUMN model_catalog.synced_by IS
    'The endpoint whose catalog sync last wrote this row; NULL for a row no '
    'sync wrote. A sync removes only rows it wrote; catalog add clears it.';
