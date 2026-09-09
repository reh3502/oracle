CREATE TABLE oracle_deployment(singleton BIGINT PRIMARY KEY CHECK(singleton=1),id TEXT NOT NULL,restored BIGINT NOT NULL CHECK(restored IN(0,1)));
CREATE TABLE oracle_guilds(id TEXT PRIMARY KEY,paused BIGINT NOT NULL CHECK(paused IN(0,1)),revision BIGINT NOT NULL CHECK(revision>0));
CREATE TABLE oracle_operations(id TEXT PRIMARY KEY,guild TEXT NOT NULL,actor TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('running','succeeded','recovery_required','failed')),revision BIGINT NOT NULL CHECK(revision>0),UNIQUE(guild,id),FOREIGN KEY(guild) REFERENCES oracle_guilds(id));
CREATE INDEX oracle_operation_recovery ON oracle_operations(guild,state,id);
CREATE TABLE oracle_effects(id TEXT PRIMARY KEY,operation TEXT NOT NULL,guild TEXT NOT NULL,purpose TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN('prepared','sent','verified','unknown','failed')),revision BIGINT NOT NULL CHECK(revision>0),receipt TEXT,UNIQUE(guild,purpose),FOREIGN KEY(guild,operation) REFERENCES oracle_operations(guild,id));
CREATE INDEX oracle_effect_recovery ON oracle_effects(guild,state,id);
CREATE INDEX oracle_effect_operation ON oracle_effects(guild,operation,state);
