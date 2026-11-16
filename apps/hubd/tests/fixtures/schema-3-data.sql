-- Representative data in the layout schema 3 used, for the upgrade tests: a paired PC helper
-- (`dhc_test_helper`) and a revoked one, a helper invite not yet used (`invite-before-upgrade`),
-- a helper page with figures, a manual page, a demo usage page and a demo page of another kind.
-- The admin's token is `dha_test_admin` and the display's `dhd_test_desk`.
INSERT INTO pages (id, title, kind, source, feed, position, stale_after_seconds, created_at,
                   quality, updated_at, data, note, stored_at) VALUES
    ('claude-max', 'Claude Max', 'meter', 'collector', 'claude-max', 0, 1800, 100,
     'provider_reported', 1794340200,
     '{"items":[{"id":"five-hour","label":"Five-hour allowance used","percent":12.0,"resets_at":"2026-11-10T22:00:00Z"},{"id":"weekly","label":"Weekly allowance used","percent":30.5}]}',
     NULL, 1794340200),
    ('words', 'Words', 'text', 'manual', NULL, 1, 86400, 101,
     'manual', 1794333600, '{"text":"Back at 3 pm"}', 'Typed in at lunch', 1794333600),
    ('chatgpt-plus', 'ChatGPT Plus', 'meter', 'demo', 'chatgpt-plus', 2, 3600, 102,
     NULL, NULL, NULL, NULL, NULL),
    ('flights', 'Flights', 'nearby-flights/1', 'demo', NULL, 3, 3600, 103,
     NULL, NULL, NULL, NULL, NULL);
INSERT INTO devices (id, name, enrolled_at, last_seen_at, info, revoked_at) VALUES
    ('d-desk', 'Desk', 200, 1794340000, NULL, NULL);
INSERT INTO display_pages (device_id, page_id) VALUES
    ('d-desk', 'claude-max'), ('d-desk', 'words'), ('d-desk', 'chatgpt-plus'), ('d-desk', 'flights');
INSERT INTO credentials (token_hash, role, subject, label, created_at, revoked_at) VALUES
    (x'2392934921664530d050386a18aa43986e24293efde8aed9f90671262da5955e', 'admin', 'a-1', 'Desktop manager', 90, NULL),
    (x'922975bcc4a6366f709c88c0facf95edccb6c277bb997be14e007bdee2356213', 'device', 'd-desk', 'Desk', 200, NULL),
    (x'77aaa9dc03f9d13654f090f027629275afb77c54b4536aa84e4b7fa35f9c7439', 'collector', 'c-pc', 'Desk PC', 210, NULL),
    (x'05', 'collector', 'c-old', 'Old laptop', 300, 310);
INSERT INTO one_time_codes (code_hash, purpose, label, created_at, expires_at, used_at) VALUES
    (x'10', 'device-enrol', 'Desk', 190, 790, 200),
    (x'79bf5407764f897b0ca641d76ca2c4a3313bb36b9a04892e38f9fce9ee675210', 'collector-pair',
     '{"label":"Laptop","services":["claude-max"]}', 1794340740, 1794341400, NULL);
INSERT INTO collector_scopes (subject, service) VALUES
    ('c-pc', 'claude-max'), ('c-pc', 'chatgpt-plus'), ('c-old', 'claude-max');
INSERT INTO audit_log (at, event, detail) VALUES (210, 'collector_paired', 'c-pc (Desk PC)');
