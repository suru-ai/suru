-- Usage no longer records a Model context window: Context Fill's capacity is
-- the one context-window figure a Session keeps. Usage rejects fields it does
-- not know, so a stored Turn keeping the old field would no longer load.
UPDATE turns
SET payload = json_remove(payload, '$.usage.model_context_window')
WHERE json_type(payload, '$.usage.model_context_window') IS NOT NULL;
