create table cron_tables (
    app_id text not null primary key,
    deployment_id text not null,
    crontab text
) strict;

create table cron_jobs (
    app_id text not null references cron_tables (app_id) on delete cascade,
    job_index integer not null check (job_index >= 0),
    schedule text not null,
    command text not null,
    has_environment integer not null check (has_environment in (0, 1)),
    primary key (app_id, job_index)
) strict;

create table cron_job_environment (
    app_id text not null,
    job_index integer not null,
    name text not null,
    value text not null,
    primary key (app_id, job_index, name),
    foreign key (app_id, job_index) references cron_jobs (app_id, job_index) on delete cascade
) strict;
