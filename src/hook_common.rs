use crate::guard::{self, Verdict};
use crate::lease::{Lease, PidChecker};

pub(crate) struct EvaluationRequest<'a> {
    pub(crate) command: &'a str,
    pub(crate) session: Option<&'a str>,
    pub(crate) leases: &'a [Lease],
    pub(crate) checker: &'a dyn PidChecker,
}

pub(crate) fn evaluate(request: EvaluationRequest<'_>) -> Verdict {
    guard::check(
        request.command,
        request.leases,
        None,
        request.session,
        request.checker,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::Lease;
    use crate::lease::test_support::AlwaysAlive;

    fn lease(port: u16, pid: u32, tag: &str, session: Option<&str>) -> Lease {
        Lease::new(port, pid, tag, session.map(str::to_owned))
    }

    fn request<'a>(
        command: &'a str,
        session: Option<&'a str>,
        leases: &'a [Lease],
        checker: &'a dyn PidChecker,
    ) -> EvaluationRequest<'a> {
        EvaluationRequest {
            command,
            session,
            leases,
            checker,
        }
    }

    #[test]
    fn allows_an_unleased_command() {
        let leases = [];

        assert_eq!(
            evaluate(request("kill 1234", None, &leases, &AlwaysAlive)),
            Verdict::Allow
        );
    }

    #[test]
    fn denies_a_foreign_live_lease() {
        let leases = [lease(3000, 1234, "dev-server", Some("other-session"))];

        assert!(matches!(
            evaluate(request(
                "kill 1234",
                Some("my-session"),
                &leases,
                &AlwaysAlive
            )),
            Verdict::Deny { .. }
        ));
    }

    #[test]
    fn allows_a_matching_session_lease() {
        let leases = [lease(3000, 1234, "dev-server", Some("my-session"))];

        assert_eq!(
            evaluate(request(
                "kill 1234",
                Some("my-session"),
                &leases,
                &AlwaysAlive
            )),
            Verdict::Allow
        );
    }

    #[test]
    fn warns_for_an_unresolvable_process_name() {
        let leases = [];

        assert!(matches!(
            evaluate(request("pkill node", None, &leases, &AlwaysAlive)),
            Verdict::Warn { .. }
        ));
    }
}
