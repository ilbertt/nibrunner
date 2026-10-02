create table cron_tables (
    app_id text not null primary key,
    record text not null,
    foreign key (app_id) references apps (app_id) on delete restrict on update restrict
) strict;
