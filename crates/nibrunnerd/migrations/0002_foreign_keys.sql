-- App identities outlive slots, instances and deletion reports, whose lifetimes differ.
create table apps (
    app_id text not null primary key
) strict;

insert into apps (app_id)
select app_id from slots
union select app_id from instances
union select app_id from activity
union select app_id from meters
union select case when json_valid(report) then json_extract(report, '$.appId') end
    from deleted_volumes
    where case when json_valid(report) then json_extract(report, '$.appId') end is not null;

create table slots_with_foreign_keys (
    app_id text    primary key,
    slot   integer not null unique,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

insert into slots_with_foreign_keys (app_id, slot)
select app_id, slot from slots;
drop table slots;
alter table slots_with_foreign_keys rename to slots;

create table instances_with_foreign_keys (
    app_id        text not null primary key,
    record        text not null,
    deployment_id text generated always as (json_extract(record, '$.deploymentId')) virtual,
    state         text generated always as (json_extract(record, '$.state')) virtual,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

insert into instances_with_foreign_keys (app_id, record)
select app_id, record from instances;
drop table instances;
alter table instances_with_foreign_keys rename to instances;

create table activity_with_foreign_keys (
    app_id            text    primary key,
    last_active_at_ms integer not null,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

insert into activity_with_foreign_keys (app_id, last_active_at_ms)
select app_id, last_active_at_ms from activity;
drop table activity;
alter table activity_with_foreign_keys rename to activity;

create table meters_with_foreign_keys (
    app_id     text    primary key,
    running_ms integer not null,
    idle_ms    integer not null,
    cpu_ms     integer not null,
    rx_bytes   integer not null,
    tx_bytes   integer not null,
    disk_provisioned_mib_seconds integer not null,
    disk_used_mib_seconds        integer not null,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

insert into meters_with_foreign_keys (app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds)
select app_id, running_ms, idle_ms, cpu_ms, rx_bytes, tx_bytes, disk_provisioned_mib_seconds, disk_used_mib_seconds from meters;
drop table meters;
alter table meters_with_foreign_keys rename to meters;

create table deleted_volumes_with_foreign_keys (
    volume_id text not null primary key,
    report    text not null,
    app_id text generated always as (
        case when json_valid(report) then json_extract(report, '$.appId') end
    ) virtual,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

insert into deleted_volumes_with_foreign_keys (volume_id, report)
select volume_id, report from deleted_volumes;
drop table deleted_volumes;
alter table deleted_volumes_with_foreign_keys rename to deleted_volumes;

create index deleted_volumes_app_id on deleted_volumes (app_id);
