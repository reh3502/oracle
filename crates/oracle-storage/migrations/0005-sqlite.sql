CREATE TABLE oracle_workflows_v5(guild TEXT NOT NULL,kind TEXT NOT NULL CHECK(kind IN('structure_plan','resource_binding','command_binding','configuration','agent_run','agent_call','agent_spend','command_group','shared_card')),key TEXT COLLATE BINARY NOT NULL,revision BIGINT NOT NULL CHECK(revision>0),value TEXT NOT NULL,PRIMARY KEY(guild,kind,key),FOREIGN KEY(guild) REFERENCES oracle_guilds(id));
INSERT INTO oracle_workflows_v5(guild,kind,key,revision,value) SELECT guild,kind,key,revision,value FROM oracle_workflows;
DROP TABLE oracle_workflows;
ALTER TABLE oracle_workflows_v5 RENAME TO oracle_workflows;
