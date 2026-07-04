create table naughty (id bigint primary key);
commit;
insert into naughty (id) values (1);

---- create above / drop below ----

drop table naughty;
