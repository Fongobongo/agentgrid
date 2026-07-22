-- Stage 13: ext профиль secret requirements (names only, never values) +
-- adapter version (capability check). secret_requirements хранятся как JSON
-- массив {env,required}; adapter_version — SemVer major string или NULL.
ALTER TABLE agent_profiles ADD COLUMN secret_requirements TEXT NOT NULL DEFAULT '[]';
ALTER TABLE agent_profiles ADD COLUMN adapter_version TEXT;
