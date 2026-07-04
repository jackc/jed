create table posts (
  id      bigint primary key,
  user_id bigint not null,
  title   text   not null
);

---- create above / drop below ----

drop table posts;
