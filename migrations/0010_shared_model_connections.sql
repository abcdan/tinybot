ALTER TABLE users ADD COLUMN codex_model TEXT;
ALTER TABLE users ADD COLUMN codex_credential TEXT;

UPDATE users
SET codex_model = model, codex_credential = credential, credential = NULL
WHERE provider = 'codex' AND credential IS NOT NULL;

-- A previously selected task model keeps its working key. The application
-- re-seals copied ciphertext on first use because its original binding differs.
UPDATE users
SET openrouter_key = COALESCE(credential, openrouter_key), credential = NULL
WHERE provider = 'openrouter';
