ALTER TABLE oracle_workflows DROP CONSTRAINT oracle_workflows_kind_check;
ALTER TABLE oracle_workflows ADD CONSTRAINT oracle_workflows_kind_check CHECK(kind IN('structure_plan','resource_binding','command_binding','configuration','agent_run','agent_call','agent_spend'));
