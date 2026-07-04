create table users (
  id    bigint primary key,
  email text   not null
);

insert into users (id, email) values (1, 'alice@example.com');
insert into users (id, email) values (2, 'bob@example.com');

---- create above / drop below ----

drop table users;
