-- What the latest similarity run made of scripts/synthetic/similarity-attack.sql,
-- once its plays are classified and weighed, `worker rank detect` and
-- `worker similarity refresh` have run (docs/rankings.md):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/similarity-attack-results.sql
--
-- `worker similarity report --artist "Similarity Target A" …` adds the same
-- computed from every play, without weights or coordination factors.

\set ON_ERROR_STOP on

\echo 'Each farm, the coordination factors its accounts got, and where its target stands'
WITH farm AS (
    SELECT st.scenario, st.target_id, st.popular_id, s.user_id
    FROM synthetic_similarity_targets st
    JOIN synthetic_scenarios s ON s.scenario = 'similarity ' || st.scenario
), factors AS (
    SELECT f.scenario, count(*) AS accounts,
           round(avg(COALESCE(a.factor, 1000)) / 1000.0, 3) AS mean_factor,
           count(*) FILTER (WHERE COALESCE(a.factor, 1000) < 950) AS under_minimum
    FROM farm f
    LEFT JOIN coordination_accounts a
           ON a.user_id = f.user_id AND a.run_id = (SELECT max(id) FROM coordination_runs)
    GROUP BY 1
), audience AS (
    SELECT st.scenario, count(DISTINCT r.user_id) AS listeners
    FROM synthetic_similarity_targets st
    JOIN ranking_daily r ON r.artist_id = st.popular_id
                        AND r.day > CURRENT_DATE - (SELECT window_to - window_from FROM similarity_runs
                                                    ORDER BY id DESC LIMIT 1)
                        AND r.weight > 0
    WHERE r.user_id NOT IN (SELECT user_id FROM synthetic_scenarios)
    GROUP BY 1
)
SELECT st.scenario, p.name AS popular, au.listeners AS honest_listeners, f.accounts,
       f.mean_factor, f.under_minimum,
       COALESCE((SELECT n.position::text || ' (' || round(n.score / 1000.0, 3) || ', '
                        || n.listeners || ' accounts)'
                 FROM artist_neighbours n
                 WHERE n.artist_id = st.popular_id AND n.neighbour_id = st.target_id), 'absent')
           AS target_in_popular_list,
       (SELECT count(*) FROM artist_neighbours n WHERE n.artist_id = st.popular_id)
           AS popular_list_length,
       (SELECT count(*) FROM user_recommendations r
        WHERE st.target_id = ANY (r.artist_ids[1:20])
          AND r.user_id NOT IN (SELECT user_id FROM synthetic_scenarios)) AS honest_users_recommended
FROM synthetic_similarity_targets st
JOIN artists p ON p.id = st.popular_id
JOIN factors f ON f.scenario = st.scenario
LEFT JOIN audience au ON au.scenario = st.scenario
ORDER BY 1;

\echo 'Lists any target entered'
SELECT a.name AS artist, t.name AS target, n.position, round(n.score / 1000.0, 3) AS score,
       n.listeners
FROM artist_neighbours n
JOIN synthetic_similarity_targets st ON st.target_id = n.neighbour_id
JOIN artists a ON a.id = n.artist_id
JOIN artists t ON t.id = n.neighbour_id
WHERE n.artist_id NOT IN (SELECT target_id FROM synthetic_similarity_targets)
ORDER BY t.name, n.position, a.name
LIMIT 40;

\echo 'Taste neighbours against the population''s genres: neighbours sharing the user''s main genre, or any'
WITH pairs AS (
    SELECT n.user_id, n.neighbour_user_id, n.position,
           g.g1 = h.g1 AS same_main,
           ARRAY[g.g1, g.g2, g.g3] && ARRAY[h.g1, h.g2, h.g3] AS any_genre
    FROM user_neighbours n
    JOIN synthetic_genres g ON g.user_id = n.user_id
    JOIN synthetic_genres h ON h.user_id = n.neighbour_user_id
)
SELECT 'all' AS neighbours, count(*) AS pairs,
       round(100.0 * avg(same_main::int), 1) AS same_main_genre_pct,
       round(100.0 * avg(any_genre::int), 1) AS a_genre_in_common_pct
FROM pairs
UNION ALL
SELECT 'first 10', count(*), round(100.0 * avg(same_main::int), 1),
       round(100.0 * avg(any_genre::int), 1)
FROM pairs WHERE position <= 10;

\echo 'Chance: two random listed accounts sharing a main genre, or any genre'
SELECT round(100.0 * avg((g.g1 = h.g1)::int), 1) AS same_main_genre_pct,
       round(100.0 * avg((ARRAY[g.g1, g.g2, g.g3] && ARRAY[h.g1, h.g2, h.g3])::int), 1)
           AS a_genre_in_common_pct
FROM (SELECT * FROM synthetic_genres ORDER BY user_id LIMIT 500) g
CROSS JOIN (SELECT * FROM synthetic_genres ORDER BY user_id DESC LIMIT 500) h;

\echo 'Recommendations in one of the user''s genres, first 20 (users have one to three of 20)'
SELECT count(*) AS recommendations,
       round(100.0 * avg((ag.genre IN (g.g1, g.g2, g.g3))::int), 1) AS in_a_genre_of_the_user_pct,
       round(100.0 * avg((ag.genre = g.g1)::int), 1) AS in_the_main_genre_pct
FROM user_recommendations r
CROSS JOIN LATERAL unnest(r.artist_ids[1:20]) AS a(artist_id)
JOIN synthetic_genres g ON g.user_id = r.user_id
JOIN synthetic_artist_genres ag ON ag.artist_id = a.artist_id;

\echo 'Similar artists in the same genre, first 10 of each list'
SELECT count(*) AS pairs, round(100.0 * avg((a.genre = b.genre)::int), 1) AS same_genre_pct
FROM artist_neighbours n
JOIN synthetic_artist_genres a ON a.artist_id = n.artist_id
JOIN synthetic_artist_genres b ON b.artist_id = n.neighbour_id
WHERE n.position <= 10;
