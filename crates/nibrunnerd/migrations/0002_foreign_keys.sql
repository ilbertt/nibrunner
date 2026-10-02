-- App identities outlive slots, instances and deletion reports, whose lifetimes differ.
create table apps (
    app_id text not null primary key
) strict;

alter table slots rename to slots_before_foreign_keys;

create table slots (
    app_id text    primary key,
    slot   integer not null unique,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

alter table instances rename to instances_before_foreign_keys;

create table instances (
    app_id        text not null primary key,
    record        text not null,
    deployment_id text generated always as (json_extract(record, '$.deploymentId')) virtual,
    state         text generated always as (json_extract(record, '$.state')) virtual,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

alter table activity rename to activity_before_foreign_keys;

create table activity (
    app_id            text    primary key,
    last_active_at_ms integer not null,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

alter table meters rename to meters_before_foreign_keys;

create table meters (
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

alter table deleted_volumes rename to deleted_volumes_before_foreign_keys;

create table deleted_volumes (
    volume_id text not null primary key,
    report    text not null,
    app_id text generated always as (
        case when json_valid(report) then json_extract(report, '$.appId') end
    ) virtual,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;

create index deleted_volumes_app_id on deleted_volumes (app_id);
