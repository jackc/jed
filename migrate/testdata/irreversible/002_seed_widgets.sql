-- An irreversible migration: no separator, so there is no down half. Migrating up
-- through it is fine; migrating down through it is an IrreversibleMigration error.
insert into widgets (id, name) values (1, 'sprocket');
insert into widgets (id, name) values (2, 'cog');
