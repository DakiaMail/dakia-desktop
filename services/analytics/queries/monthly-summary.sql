-- Private aggregate-only product report. Small dimension buckets are omitted
-- to reduce re-identification risk. The monthly total remains useful for the
-- opted-in active-install estimate and contains no user-level rows.
SELECT month, SUM(active_installs) AS estimated_active_installs
FROM monthly_usage_aggregates
GROUP BY month
ORDER BY month DESC;

SELECT month, app_version, SUM(active_installs) AS estimated_active_installs
FROM monthly_usage_aggregates
GROUP BY month, app_version
HAVING SUM(active_installs) >= 20
ORDER BY month DESC, estimated_active_installs DESC;

SELECT month, os_version, arch,
       SUM(active_installs) AS estimated_active_installs
FROM monthly_usage_aggregates
GROUP BY month, os_version, arch
HAVING SUM(active_installs) >= 20
ORDER BY month DESC, estimated_active_installs DESC;

WITH provider_names(provider) AS (
  VALUES ('fastmail'), ('gmail'), ('icloud'), ('outlook'), ('yahoo'), ('other')
)
SELECT aggregates.month, provider_names.provider,
       SUM(aggregates.active_installs) AS estimated_active_installs
FROM monthly_usage_aggregates AS aggregates
JOIN provider_names
  ON ',' || aggregates.providers || ','
     LIKE '%,' || provider_names.provider || ',%'
GROUP BY aggregates.month, provider_names.provider
HAVING SUM(aggregates.active_installs) >= 20
ORDER BY aggregates.month DESC, estimated_active_installs DESC;
