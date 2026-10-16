-- Errors stored before request URLs were left out of them hold the URL:
-- for Last.fm, its query string with the API key and a user's session key
-- (`sk`), which the import's error message showed that user.
UPDATE scrobble_imports
SET error_message = regexp_replace(error_message, ' for url \([^)]*\)', '', 'g')
WHERE error_message LIKE '% for url (%';

UPDATE enrichment_jobs
SET last_error = regexp_replace(last_error, ' for url \([^)]*\)', '', 'g')
WHERE last_error LIKE '% for url (%';

UPDATE worker_heartbeats
SET last_error = regexp_replace(last_error, ' for url \([^)]*\)', '', 'g')
WHERE last_error LIKE '% for url (%';
