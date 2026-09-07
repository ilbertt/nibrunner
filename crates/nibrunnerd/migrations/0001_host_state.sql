-- What this host keeps about itself, and nothing about what it is *told* to be.
--
-- `desired.json` and `reported.json` stay files on purpose: one is written by whoever deploys and
-- the other is what anything reads to see status, so both are the interface rather than storage.
-- Everything here is the daemon's own note to itself, and the reason it is one database rather
-- than five documents is that a pass writes all of it and a crash must not leave half.

-- The identity this host reports under, decided once and never again: a host that renamed itself
-- on restart would look like a second host to whatever is counting them.
create table host_identity (
    only_row integer primary key check (only_row = 0),
    host_id  text    not null
) strict;

-- One integer per app, and every per-app resource derives from it: the loopback port, the tap, the
-- addresses, the MAC, the NBD minor. It outlives a deploy on purpose — a redeploy that moved an
-- app's slot would move its port, and nothing routing to it would know.
create table slots (
    app_id text    primary key,
    slot   integer not null unique
) strict;

-- Where the next search for a free slot begins. Apart from the table rather than derived from it,
-- because a slot released and immediately reused is a port a client still holds a connection to.
create table slot_cursor (
    only_row integer primary key check (only_row = 0),
    cursor   integer not null
) strict;

-- The record is JSON rather than a column per field. Nothing queries into it — the daemon loads
-- every record on the way up and works in memory — so normalising it would be thirty columns and
-- four child tables serving no reader. What is a column is what an operator would want to see
-- without a JSON tool, and those are generated from the record so the two cannot disagree.
create table instances (
    app_id        text not null primary key,
    record        text not null,
    deployment_id text generated always as (json_extract(record, '$.deploymentId')) virtual,
    state         text generated always as (json_extract(record, '$.state')) virtual
) strict;

-- Only the moment, never the counts it was derived from: the kernel's counters do not outlive the
-- daemon either, because the first apply after a restart rewrites the table.
create table activity (
    app_id            text    primary key,
    last_active_at_ms integer not null
) strict;

-- Deletions this host carried out that whatever reads the report has not yet acknowledged.
--
-- Kept because the removal is not re-derivable: the volume is gone, so the next observation sees
-- nothing, and nothing is exactly what an app whose filesystem lives on another host looks like.
-- Only the host that did the removal can say it happened.
create table deleted_volumes (
    volume_id text not null primary key,
    report    text not null
) strict;
