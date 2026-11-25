CREATE TABLE project_spaces (
    id TEXT PRIMARY KEY,
    owner TEXT NOT NULL REFERENCES users(id),
    title TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL
);

CREATE TABLE project_members (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL REFERENCES project_spaces(id) ON DELETE CASCADE,
    user TEXT NOT NULL REFERENCES users(id),
    role TEXT NOT NULL CHECK(role IN ('owner','editor','viewer')),
    joined_at INTEGER,
    UNIQUE(project,user)
);

CREATE INDEX project_members_user ON project_members(user, project);

CREATE VIEW active_project_members AS
SELECT m.id, m.project, m.user, m.role, u.display_name
FROM project_members m JOIN project_spaces p ON p.id=m.project
JOIN users u ON u.id=m.user AND u.suspended_at IS NULL
JOIN users owner ON owner.id=p.owner AND owner.suspended_at IS NULL
WHERE m.joined_at IS NOT NULL;

CREATE TABLE project_items (
    id TEXT PRIMARY KEY,
    project TEXT NOT NULL REFERENCES project_spaces(id) ON DELETE CASCADE,
    author TEXT NOT NULL REFERENCES users(id),
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('note','task')),
    source_revision INTEGER,
    created_at INTEGER NOT NULL,
    assignee TEXT REFERENCES project_members(id) ON DELETE SET NULL,
    done INTEGER NOT NULL DEFAULT 0 CHECK(done IN (0,1)),
    revision INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX project_items_latest ON project_items(project, created_at DESC, id);

CREATE TABLE project_item_audience (
    item TEXT NOT NULL REFERENCES project_items(id) ON DELETE CASCADE,
    member TEXT NOT NULL REFERENCES project_members(id) ON DELETE CASCADE,
    PRIMARY KEY(item, member)
);

CREATE INDEX project_item_audience_member ON project_item_audience(member);
