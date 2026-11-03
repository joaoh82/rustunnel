//! Persist in-memory tunnel counters to `tunnel_log`.
//!
//! Writes are absolute and monotonic. `GREATEST` means a flush and a close
//! that race, or a stale snapshot that lands after a newer one, cannot
//! double-count and cannot decrease a counter. `bytes_proxied` is rewritten
//! from the directional columns so it stays equal to `bytes_in + bytes_out`
//! for every row this process updates.
//!
//! Flushes only touch rows that are still open (`unregistered_at IS NULL`).
//! A flush that loses the race with close is a no-op.

use chrono::Utc;
use sqlx::PgPool;

use crate::core::{LiveTunnelMeter, TunnelCore};
use crate::error::Result;

fn as_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Write absolute counters onto the open `tunnel_log` row. Does not close it.
pub async fn flush_tunnel_meters(pool: &PgPool, meters: &LiveTunnelMeter) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE tunnel_log SET \
            request_count = GREATEST(request_count, $1), \
            bytes_in = GREATEST(bytes_in, $2), \
            bytes_out = GREATEST(bytes_out, $3), \
            bytes_proxied = GREATEST(bytes_in, $2) + GREATEST(bytes_out, $3) \
         WHERE tunnel_id = $4 AND unregistered_at IS NULL",
    )
    .bind(as_i64(meters.request_count))
    .bind(as_i64(meters.bytes_in))
    .bind(as_i64(meters.bytes_out))
    .bind(meters.tunnel_id.to_string())
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows)
}

/// Close the open row and store absolute counters.
///
/// A second call matches nothing. A snapshot smaller than what is already
/// stored does not decrease the columns, and `bytes_proxied` is the sum of
/// the stored directional values.
pub async fn log_tunnel_unregistered(pool: &PgPool, meters: &LiveTunnelMeter) -> Result<()> {
    sqlx::query(
        "UPDATE tunnel_log SET \
            unregistered_at = $1, \
            request_count = GREATEST(request_count, $2), \
            bytes_in = GREATEST(bytes_in, $3), \
            bytes_out = GREATEST(bytes_out, $4), \
            bytes_proxied = GREATEST(bytes_in, $3) + GREATEST(bytes_out, $4) \
         WHERE tunnel_id = $5 AND unregistered_at IS NULL",
    )
    .bind(Utc::now())
    .bind(as_i64(meters.request_count))
    .bind(as_i64(meters.bytes_in))
    .bind(as_i64(meters.bytes_out))
    .bind(meters.tunnel_id.to_string())
    .execute(pool)
    .await?;
    Ok(())
}

/// Flush every tunnel currently registered in this process.
///
/// This is the periodic flush and the first step of graceful shutdown.
/// Rows stay open.
pub async fn flush_live_meters(pool: &PgPool, core: &TunnelCore) -> Result<u64> {
    let mut updated = 0u64;
    for meters in core.live_tunnel_meters() {
        updated += flush_tunnel_meters(pool, &meters).await?;
    }
    Ok(updated)
}

/// Flush, then close, every tunnel still registered in this process.
///
/// Called on SIGTERM/SIGINT after sessions have been asked to exit, so a
/// session that did not reach its own unregister still persists counters
/// before the process leaves. Already-closed rows are left alone.
pub async fn flush_and_close_live_meters(pool: &PgPool, core: &TunnelCore) -> Result<u64> {
    let meters = core.live_tunnel_meters();
    let mut closed = 0u64;
    for meter in &meters {
        flush_tunnel_meters(pool, meter).await?;
        log_tunnel_unregistered(pool, meter).await?;
        closed += 1;
    }
    Ok(closed)
}

/// Mark this region's still-open `tunnel_log` rows closed.
///
/// Does not write counters. A periodic flush (or the shutdown flush) has
/// already stored them, and this statement must not zero that work.
/// Other regions, and rows with a NULL `region_id`, are not touched.
pub async fn close_stale_tunnels(pool: &PgPool, region_id: &str) -> Result<u64> {
    let rows = sqlx::query(
        "UPDATE tunnel_log SET unregistered_at = $1 \
         WHERE unregistered_at IS NULL AND region_id = $2",
    )
    .bind(Utc::now())
    .bind(region_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;

    use tokio::sync::mpsc;
    use uuid::Uuid;

    use super::*;
    use crate::config::DatabaseSection;
    use crate::core::TunnelCore;
    use crate::db::{self, list_user_tunnels, TunnelRegistration};

    async fn pool() -> PgPool {
        let url = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://rustunnel:test@localhost:5432/rustunnel_test".to_string()
        });
        let db = db::init_db(&DatabaseSection {
            url,
            captured_path: ":memory:".into(),
        })
        .await
        .expect("init_db");
        db.pg
    }

    async fn insert_open(pool: &PgPool, tunnel_id: Uuid, region: &str, user_id: Option<Uuid>) {
        db::log_tunnel_registered(
            pool,
            TunnelRegistration {
                tunnel_id: &tunnel_id.to_string(),
                protocol: "http",
                label: "meter-test",
                session_id: &Uuid::new_v4().to_string(),
                token_id: None,
                region_id: region,
                user_id,
            },
        )
        .await
        .expect("register");
    }

    #[derive(sqlx::FromRow)]
    struct MeterRow {
        request_count: i64,
        bytes_in: i64,
        bytes_out: i64,
        bytes_proxied: i64,
        open: bool,
    }

    async fn read_row(pool: &PgPool, tunnel_id: Uuid) -> MeterRow {
        sqlx::query_as(
            "SELECT request_count, bytes_in, bytes_out, bytes_proxied, \
                    (unregistered_at IS NULL) AS open \
             FROM tunnel_log WHERE tunnel_id = $1",
        )
        .bind(tunnel_id.to_string())
        .fetch_one(pool)
        .await
        .expect("row")
    }

    fn assert_equal_split(row: &MeterRow, requests: i64, bytes_in: i64, bytes_out: i64) {
        assert_eq!(row.request_count, requests);
        assert_eq!(row.bytes_in, bytes_in);
        assert_eq!(row.bytes_out, bytes_out);
        assert_eq!(row.bytes_proxied, bytes_in + bytes_out);
    }

    fn meter(tunnel_id: Uuid, requests: u64, bytes_in: u64, bytes_out: u64) -> LiveTunnelMeter {
        LiveTunnelMeter {
            tunnel_id,
            request_count: requests,
            bytes_in,
            bytes_out,
        }
    }

    #[tokio::test]
    async fn flush_on_close_persists_counters_without_double_count() {
        let pool = pool().await;
        let tunnel_id = Uuid::new_v4();
        insert_open(&pool, tunnel_id, "meter-close", None).await;

        log_tunnel_unregistered(&pool, &meter(tunnel_id, 4, 100, 40))
            .await
            .unwrap();
        let row = read_row(&pool, tunnel_id).await;
        assert!(!row.open);
        assert_equal_split(&row, 4, 100, 40);

        // A second close, and a later flush, must not add the numbers again.
        log_tunnel_unregistered(&pool, &meter(tunnel_id, 4, 100, 40))
            .await
            .unwrap();
        flush_tunnel_meters(&pool, &meter(tunnel_id, 999, 500, 500))
            .await
            .unwrap();
        let row = read_row(&pool, tunnel_id).await;
        assert_equal_split(&row, 4, 100, 40);

        // A stale smaller snapshot cannot decrease a value already stored
        // by an earlier flush on the still-open row.
        let open_id = Uuid::new_v4();
        insert_open(&pool, open_id, "meter-close", None).await;
        flush_tunnel_meters(&pool, &meter(open_id, 9, 100, 50))
            .await
            .unwrap();
        log_tunnel_unregistered(&pool, &meter(open_id, 3, 80, 20))
            .await
            .unwrap();
        let row = read_row(&pool, open_id).await;
        assert!(!row.open);
        assert_equal_split(&row, 9, 100, 50);

        // A close with a higher snapshot replaces, it does not add.
        let higher_id = Uuid::new_v4();
        insert_open(&pool, higher_id, "meter-close", None).await;
        flush_tunnel_meters(&pool, &meter(higher_id, 2, 10, 10))
            .await
            .unwrap();
        log_tunnel_unregistered(&pool, &meter(higher_id, 5, 120, 70))
            .await
            .unwrap();
        let row = read_row(&pool, higher_id).await;
        assert_equal_split(&row, 5, 120, 70);
    }

    #[tokio::test]
    async fn periodic_flush_persists_counters_while_tunnel_is_open() {
        let pool = pool().await;
        let core = TunnelCore::new([0, 0], [0, 0], 4, 4, 0, 10);
        let (tx, _rx) = mpsc::channel(1);
        let session_id = core.register_session(
            "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
            String::new(),
            None,
            None,
            tx,
        );
        let (tunnel_id, subdomain) = core
            .register_http_tunnel(
                &session_id,
                Some("mtrflush".into()),
                rustunnel_protocol::TunnelProtocol::Http,
                None,
            )
            .unwrap();
        insert_open(&pool, tunnel_id, "meter-periodic", None).await;

        {
            let group = core.http_routes.get(&subdomain).unwrap();
            let member = group.members.get(&tunnel_id).unwrap();
            member.info.add_transfer(200, 25);
            member.info.request_count.fetch_add(6, Ordering::Relaxed);
        }

        let updated = flush_live_meters(&pool, &core).await.unwrap();
        assert_eq!(updated, 1);
        let row = read_row(&pool, tunnel_id).await;
        assert!(row.open, "periodic flush must leave the tunnel open");
        assert_equal_split(&row, 6, 200, 25);

        {
            let group = core.http_routes.get(&subdomain).unwrap();
            let member = group.members.get(&tunnel_id).unwrap();
            member.info.add_transfer(50, 5);
            member.info.request_count.fetch_add(1, Ordering::Relaxed);
        }
        flush_live_meters(&pool, &core).await.unwrap();
        let row = read_row(&pool, tunnel_id).await;
        assert!(row.open);
        // 250/30, not 200+250 / 25+30.
        assert_equal_split(&row, 7, 250, 30);
    }

    #[tokio::test]
    async fn graceful_shutdown_flushes_before_closing_rows() {
        let pool = pool().await;
        let core = TunnelCore::new([0, 0], [0, 0], 4, 4, 0, 10);
        let (tx, _rx) = mpsc::channel(1);
        let session_id = core.register_session(
            "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
            String::new(),
            None,
            None,
            tx,
        );
        let (tunnel_id, subdomain) = core
            .register_http_tunnel(
                &session_id,
                Some("mtrstop".into()),
                rustunnel_protocol::TunnelProtocol::Http,
                None,
            )
            .unwrap();
        insert_open(&pool, tunnel_id, "meter-shutdown", None).await;
        {
            let group = core.http_routes.get(&subdomain).unwrap();
            let member = group.members.get(&tunnel_id).unwrap();
            member.info.add_transfer(1000, 250);
            member.info.request_count.fetch_add(11, Ordering::Relaxed);
        }

        // Same call main makes when SIGTERM/SIGINT arrives, before sessions
        // close their rows.
        flush_live_meters(&pool, &core).await.unwrap();
        let row = read_row(&pool, tunnel_id).await;
        assert!(row.open);
        assert_equal_split(&row, 11, 1000, 250);

        // Session teardown then closes with the same absolute snapshot.
        let snapshot = core.tunnel_meter(&tunnel_id);
        log_tunnel_unregistered(&pool, &snapshot).await.unwrap();
        let row = read_row(&pool, tunnel_id).await;
        assert!(!row.open);
        assert_equal_split(&row, 11, 1000, 250);

        // Leftover live tunnels are flushed and closed even if the session
        // task never reached its own unregister.
        let (other_id, other_sub) = core
            .register_http_tunnel(
                &session_id,
                Some("mtrleft".into()),
                rustunnel_protocol::TunnelProtocol::Http,
                None,
            )
            .unwrap();
        insert_open(&pool, other_id, "meter-shutdown", None).await;
        {
            let group = core.http_routes.get(&other_sub).unwrap();
            let member = group.members.get(&other_id).unwrap();
            member.info.add_transfer(8, 13);
        }
        let closed = flush_and_close_live_meters(&pool, &core).await.unwrap();
        assert!(closed >= 1);
        let row = read_row(&pool, other_id).await;
        assert!(!row.open);
        assert_equal_split(&row, 0, 8, 13);
        // The tunnel already closed above is not doubled by the sweep.
        let row = read_row(&pool, tunnel_id).await;
        assert_equal_split(&row, 11, 1000, 250);
    }

    #[tokio::test]
    async fn close_stale_tunnels_only_touches_its_own_region() {
        let pool = pool().await;
        let suffix = Uuid::new_v4().simple().to_string();
        let own = format!("region-own-{suffix}");
        let other = format!("region-other-{suffix}");
        let own_id = Uuid::new_v4();
        let other_id = Uuid::new_v4();
        let null_id = Uuid::new_v4();
        insert_open(&pool, own_id, &own, None).await;
        insert_open(&pool, other_id, &other, None).await;
        insert_open(&pool, null_id, "temp", None).await;
        sqlx::query(
            "UPDATE tunnel_log SET region_id = NULL, bytes_proxied = 42, bytes_in = 1, bytes_out = 2 \
             WHERE tunnel_id = $1",
        )
        .bind(null_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE tunnel_log SET bytes_proxied = 42, bytes_in = 1, bytes_out = 2 \
             WHERE tunnel_id = $1",
        )
        .bind(own_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

        let closed = close_stale_tunnels(&pool, &own).await.unwrap();
        assert_eq!(closed, 1);

        let own_row = read_row(&pool, own_id).await;
        assert!(!own_row.open);
        // Stale close must not rewrite counters, even when they are not a split.
        assert_eq!(own_row.bytes_proxied, 42);
        assert_eq!(own_row.bytes_in, 1);
        assert_eq!(own_row.bytes_out, 2);

        assert!(read_row(&pool, other_id).await.open);
        let null_row = read_row(&pool, null_id).await;
        assert!(null_row.open);
        assert_eq!(null_row.bytes_proxied, 42);
    }

    #[tokio::test]
    async fn cap_and_usage_reads_keep_bytes_proxied_totals() {
        let pool = pool().await;
        let user_id = Uuid::new_v4();
        let email = format!("meter-{user_id}@example.com");
        sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')")
            .bind(user_id)
            .bind(&email)
            .execute(&pool)
            .await
            .unwrap();

        // New close: directional columns, proxied is the sum.
        let closed_id = Uuid::new_v4();
        insert_open(&pool, closed_id, "meter-usage", Some(user_id)).await;
        log_tunnel_unregistered(&pool, &meter(closed_id, 3, 400, 600))
            .await
            .unwrap();

        // Still open, counters flushed. The Hobby month sum includes it.
        let open_id = Uuid::new_v4();
        insert_open(&pool, open_id, "meter-usage", Some(user_id)).await;
        flush_tunnel_meters(&pool, &meter(open_id, 1, 100, 150))
            .await
            .unwrap();

        // Legacy row: only bytes_proxied was stored. Readers must still see 500,
        // not bytes_in + bytes_out (0).
        let legacy_id = Uuid::new_v4();
        insert_open(&pool, legacy_id, "meter-usage", Some(user_id)).await;
        sqlx::query(
            "UPDATE tunnel_log SET unregistered_at = now(), bytes_proxied = 500, request_count = 8 \
             WHERE tunnel_id = $1",
        )
        .bind(legacy_id.to_string())
        .execute(&pool)
        .await
        .unwrap();

        // Another user's bytes must not leak into this user's cap.
        let stranger = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, email, password_hash) VALUES ($1, $2, 'x')")
            .bind(stranger)
            .bind(format!("stranger-{stranger}@example.com"))
            .execute(&pool)
            .await
            .unwrap();
        let stranger_tunnel = Uuid::new_v4();
        insert_open(&pool, stranger_tunnel, "meter-usage", Some(stranger)).await;
        log_tunnel_unregistered(&pool, &meter(stranger_tunnel, 1, 9_000, 9_000))
            .await
            .unwrap();

        let month = crate::billing::month_bytes_proxied(&pool, user_id)
            .await
            .unwrap();
        // 1000 (closed) + 250 (open, flushed) + 500 (legacy proxied).
        assert_eq!(month, 1_750);

        let (proxied, inn, out): (i64, i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint, \
                    COALESCE(SUM(bytes_in), 0)::bigint, \
                    COALESCE(SUM(bytes_out), 0)::bigint \
             FROM tunnel_log \
             WHERE user_id = $1 AND registered_at >= CURRENT_DATE",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        // Same expression as get_platform_usage bandwidth_today_bytes and the
        // PAYG spend-cap SUM(bytes_proxied), scoped to this user.
        assert_eq!(proxied, 1_750);
        assert_eq!(inn, 500);
        assert_eq!(out, 750);
        assert_ne!(proxied, inn);
        assert_ne!(proxied, out);

        let admin = list_user_tunnels(&pool, &user_id, 20, 0).await.unwrap();
        let admin_total: i64 = admin.iter().map(|row| row.bytes_proxied).sum();
        assert_eq!(admin_total, 1_750);
        let closed = admin
            .iter()
            .find(|row| row.tunnel_id == closed_id.to_string())
            .unwrap();
        assert_eq!(closed.bytes_proxied, closed.bytes_in + closed.bytes_out);
        let legacy = admin
            .iter()
            .find(|row| row.tunnel_id == legacy_id.to_string())
            .unwrap();
        assert_eq!(legacy.bytes_proxied, 500);
        assert_eq!(legacy.bytes_in, 0);
        assert_eq!(legacy.bytes_out, 0);

        sqlx::query(
            "INSERT INTO usage_events \
                (user_id, tunnel_id, period_start, period_end, bytes_proxied, bytes_in, bytes_out) \
             VALUES ($1, 'slice-new', now() - interval '1 hour', now(), 40, 10, 30)",
        )
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO usage_events \
                (user_id, tunnel_id, period_start, period_end, bytes_proxied) \
             VALUES ($1, 'slice-legacy', now() - interval '2 hour', now() - interval '1 hour', 70)",
        )
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
        let (slice_proxied, slice_sum): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(bytes_proxied), 0)::bigint, \
                    COALESCE(SUM(bytes_in + bytes_out), 0)::bigint \
             FROM usage_events WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(slice_proxied, 110);
        assert_eq!(slice_sum, 40);

        sqlx::query("DELETE FROM usage_events WHERE user_id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tunnel_log WHERE user_id = $1 OR user_id = $2")
            .bind(user_id)
            .bind(stranger)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM users WHERE id = $1 OR id = $2")
            .bind(user_id)
            .bind(stranger)
            .execute(&pool)
            .await
            .unwrap();
    }
}
