-- What the latest coordination run did to each scenario of
-- scripts/synthetic/coordination.sql, once its plays are classified and
-- weighed and `worker rank detect` has run (docs/rankings.md):
--
--   psql "$SCRATCH_URL" -f scripts/synthetic/coordination-results.sql
--
-- Honest accounts are the population's and the hype signups; the friends,
-- households, campus, release listeners and fans are among them.

\set ON_ERROR_STOP on

CREATE TEMP TABLE scenario_groups AS
SELECT user_id, scenario FROM synthetic_scenarios
UNION ALL
SELECT id, 'honest: everyone' FROM users WHERE username ~ '^(synth|hype)[0-9]+$';

\echo 'Accounts per scenario, and the factors the latest run gave them'
SELECT g.scenario, count(*) AS accounts, count(a.user_id) AS flagged,
       count(*) FILTER (WHERE a.factor = 0) AS at_zero,
       round(avg(COALESCE(a.factor, 1000)) / 1000.0, 3) AS mean_factor,
       round(min(COALESCE(a.factor, 1000)) / 1000.0, 3) AS min_factor
FROM scenario_groups g
LEFT JOIN coordination_accounts a
       ON a.user_id = g.user_id AND a.run_id = (SELECT max(id) FROM coordination_runs)
GROUP BY 1 ORDER BY 1;

\echo 'Weight each scenario keeps after the factors, last 7 and 28 days'
SELECT g.scenario,
       round(100.0 * sum(k.kept) FILTER (WHERE d.day >= CURRENT_DATE - 6)
             / NULLIF(sum(d.weight) FILTER (WHERE d.day >= CURRENT_DATE - 6), 0), 2) AS week_pct,
       round(100.0 * sum(k.kept) / NULLIF(sum(d.weight), 0), 2) AS month_pct
FROM scenario_groups g
JOIN ranking_daily d ON d.user_id = g.user_id AND d.day >= CURRENT_DATE - 27
LEFT JOIN coordination_days c ON c.user_id = d.user_id AND c.day = d.day
CROSS JOIN LATERAL (SELECT d.weight::bigint * COALESCE(c.factor, 1000) / 1000 AS kept) k
GROUP BY 1 ORDER BY 1;

\echo 'Honest accounts with a factor below 1'
SELECT CASE WHEN a.factor >= 990 THEN 'a: 0.99 or more'
            WHEN a.factor >= 900 THEN 'b: 0.90 to 0.99'
            WHEN a.factor >= 500 THEN 'c: 0.50 to 0.90'
            ELSE 'd: under 0.50' END AS factor,
       count(*) AS accounts
FROM coordination_accounts a
JOIN users u ON u.id = a.user_id
WHERE a.run_id = (SELECT max(id) FROM coordination_runs) AND u.username ~ '^(synth|hype)[0-9]+$'
GROUP BY 1 ORDER BY 1;

\echo 'Each scenario''s artists in the week by listeners, before and after the factors'
WITH per_user AS (
    SELECT d.user_id, d.artist_id AS entity_id, sum(d.weight)::bigint AS weight,
           sum(d.weight::bigint * COALESCE(c.factor, 1000) / 1000)::bigint AS coordinated
    FROM ranking_daily d
    LEFT JOIN coordination_days c ON c.user_id = d.user_id AND c.day = d.day
    WHERE d.day BETWEEN CURRENT_DATE - 6 AND CURRENT_DATE
    GROUP BY 1, 2
), per_entity AS (
    SELECT entity_id,
           sum(LEAST(1000, weight * 1000 / 250)) AS before,
           sum(CASE WHEN weight > 0 THEN LEAST(1000, weight * 1000 / 250) * coordinated / weight
                    ELSE 0 END) AS after,
           sum(weight) AS weight_before, sum(coordinated) AS weight_after
    FROM per_user
    GROUP BY 1
), ranked AS (
    SELECT *,
           row_number() OVER (ORDER BY before DESC, weight_before DESC, entity_id) AS rank_before,
           row_number() OVER (ORDER BY after DESC, weight_after DESC, entity_id) AS rank_after,
           count(*) FILTER (WHERE weight_after > 0) OVER () AS ranked
    FROM per_entity
)
SELECT st.scenario, a.name,
       r.rank_before, round(r.before / 1000.0, 1) AS listeners_before,
       CASE WHEN r.weight_after > 0 THEN r.rank_after::text ELSE 'out' END AS rank_after,
       round(r.after / 1000.0, 1) AS listeners_after, r.ranked AS of
FROM synthetic_targets st
JOIN artists a ON a.id = st.artist_id
JOIN ranked r ON r.entity_id = st.artist_id
ORDER BY st.scenario, a.name;
