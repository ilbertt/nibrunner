-- SQLite can add a reference through a virtual column without rebuilding the table.
alter table instances add column slot_app_id text
    generated always as (app_id) virtual not null
    references slots (app_id) on delete cascade on update restrict;
