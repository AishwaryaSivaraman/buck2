/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use buck2_common::manifold::Ttl;
use buck2_core::buck2_env;
use buck2_events::metadata::username;

const SCHEDULE_TYPE_CONTINUOUS: &str = "continuous";

// Copied from "Is User Command" from scuba buck2_builds
const ROBOTS: &[&str] = &[
    "twsvcscm",
    "svcscm",
    "facebook",
    "root",
    "svc-si_admin",
    "svc-fbsi_datamgr",
];

const USER_TTL_DAYS: u64 = 365;
const DEFAULT_TTL_DAYS: u64 = 60;
// diff signal retention is 4 weeks
const CI_EXCEPT_CONTINUOUS_TTL_DAYS: u64 = 28;

pub fn manifold_event_log_ttl() -> anyhow::Result<Ttl> {
    manifold_event_log_ttl_impl(ROBOTS, username().ok().flatten(), schedule_type()?)
}

fn manifold_event_log_ttl_impl(
    robots: &[&str],
    username: Option<String>,
    schedule_type: Option<&'static str>,
) -> anyhow::Result<Ttl> {
    // 1. return if this is a test
    let env = buck2_env!("BUCK2_TEST_MANIFOLD_TTL_S", type=u64, applicability=testing)?;
    if let Some(env) = env {
        return Ok::<Ttl, anyhow::Error>(Ttl::from_secs(env));
    }

    // 2. return if this is a user
    if let Some(username) = username {
        if !robots.contains(&(username.as_str())) {
            return Ok::<Ttl, anyhow::Error>(Ttl::from_days(USER_TTL_DAYS));
        }
    }

    // 3. return if it's not continuous
    if let Some(sched) = schedule_type {
        if sched != SCHEDULE_TYPE_CONTINUOUS {
            return Ok(Ttl::from_days(CI_EXCEPT_CONTINUOUS_TTL_DAYS));
        }
    }

    // 4. use default
    Ok::<Ttl, anyhow::Error>(Ttl::from_days(DEFAULT_TTL_DAYS))
}

fn schedule_type() -> anyhow::Result<Option<&'static str>> {
    // Same as RE does https://fburl.com/code/sj13r130
    if let Some(env) = buck2_env!("SCHEDULE_TYPE", applicability = internal)? {
        Ok(Some(env))
    } else {
        buck2_env!("SANDCASTLE_SCHEDULE_TYPE", applicability = internal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_a_user() -> anyhow::Result<()> {
        assert_eq!(
            manifold_event_log_ttl_impl(
                &["twsvcscm"],
                Some("random_person".to_owned()),
                Some("continuous")
            )?
            .as_secs(),
            365 * 24 * 60 * 60,
        );
        Ok(())
    }

    #[test]
    fn test_not_a_user() -> anyhow::Result<()> {
        assert_eq!(
            manifold_event_log_ttl_impl(&["twsvcscm"], Some("twsvcscm".to_owned()), None)?
                .as_secs(),
            60 * 24 * 60 * 60,
        );
        Ok(())
    }

    #[test]
    fn test_not_a_user_and_not_continuous() -> anyhow::Result<()> {
        assert_eq!(
            manifold_event_log_ttl_impl(&["twsvcscm"], Some("twsvcscm".to_owned()), Some("foo"))?
                .as_secs(),
            28 * 24 * 60 * 60,
        );
        Ok(())
    }

    #[test]
    fn test_not_a_user_and_continuous() -> anyhow::Result<()> {
        assert_eq!(
            manifold_event_log_ttl_impl(
                &["twsvcscm"],
                Some("twsvcscm".to_owned()),
                Some("continuous")
            )?
            .as_secs(),
            60 * 24 * 60 * 60,
        );
        Ok(())
    }
}
