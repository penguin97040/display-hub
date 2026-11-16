-- Representative data in the layout schema 2 used, for the upgrade tests. The pages it gives
-- before the upgrade are in schema-2-pages.json.
INSERT INTO subscriptions (id, service, title, source, position, created_at) VALUES
    ('claude-max', 'claude-max', 'Claude Max', 'collector', 2, 100),
    ('chatgpt-plus', 'chatgpt-plus', 'ChatGPT Plus', 'none', 4, 101),
    ('opencode-go-plus', 'opencode-go-plus', 'OpenCode Go Plus', 'demo', 0, 102),
    ('work-claude', 'claude-max', 'Work Claude', 'manual', 1, 103),
    ('chatgpt-plus-2', 'chatgpt-plus', 'Spare ChatGPT', 'manual', 5, 104),
    ('opencode-go-plus-2', 'opencode-go-plus', 'Team OpenCode', 'collector', 3, 105),
    ('chatgpt-plus-3', 'chatgpt-plus', 'Old ChatGPT', 'collector', 6, 106),
    ('claude-max-2', 'claude-max', 'Empty manual', 'manual', 7, 107),
    ('claude-max-3', 'claude-max', 'Demo Claude', 'demo', 8, 108);
INSERT INTO page_data (subscription_id, quality, updated_at, metrics, note, stored_at) VALUES
    ('claude-max', 'provider_reported', 1794340200,
     '[{"window":"five-hour","used_percent":80.0,"resets_at":"2026-11-10T19:00:00Z"},{"window":"weekly","used_percent":30.5,"resets_at":"2026-11-11T20:00:00Z"}]',
     NULL, 1794340200),
    ('work-claude', 'manual', 1794333600,
     '[{"window":"five-hour","used_percent":12.3,"resets_at":"2026-11-10T21:00:00Z"},{"window":"weekly","used_percent":null,"resets_at":null}]',
     'Typed in at lunch', 1794333600),
    ('chatgpt-plus-3', 'provider_reported', 1794250800,
     '[{"window":"five-hour","used_percent":99.9,"resets_at":"2026-11-10T20:01:40Z"},{"window":"weekly","used_percent":45.0,"resets_at":"2026-11-16T00:00:00Z"}]',
     NULL, 1794250800),
    ('claude-max-2', 'manual', 1794340740, '[]', NULL, 1794340740);
INSERT INTO devices (id, name, enrolled_at, last_seen_at, info, revoked_at) VALUES
    ('d-desk', 'Desk', 200, 1794340000, NULL, NULL),
    ('d-kitchen', 'Kitchen', 201, NULL, NULL, NULL),
    ('d-old', 'Old', 150, 160, NULL, 170);
INSERT INTO credentials (token_hash, role, subject, label, created_at, revoked_at) VALUES
    (x'01', 'admin', 'a-1', 'Desktop manager', 90, NULL),
    (x'02', 'device', 'd-desk', 'Desk', 200, NULL),
    (x'03', 'device', 'd-kitchen', 'Kitchen', 201, NULL),
    (x'04', 'device', 'd-old', 'Old', 150, 170),
    (x'05', 'collector', 'c-pc', 'PC helper', 210, NULL);
INSERT INTO one_time_codes (code_hash, purpose, label, created_at, expires_at, used_at) VALUES
    (x'10', 'device-enrol', 'Desk', 190, 790, 200);
INSERT INTO collector_scopes (subject, service) VALUES ('c-pc', 'claude-max');
INSERT INTO audit_log (at, event, detail) VALUES (90, 'admin_paired', 'a-1 (Desktop manager)');

