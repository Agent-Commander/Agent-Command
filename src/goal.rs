pub(crate) const GOAL_MET: &str = "GOAL_MET";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Goal {
    pub(crate) member: String,
    pub(crate) criteria: String,
    pub(crate) turns: u32,
    pub(crate) cap: u32,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Step {
    Continue(String),
    Met,
    CapReached,
    Failed(String),
}

impl Goal {
    pub(crate) fn new(member: &str, criteria: &str, cap: u32) -> Self {
        Self {
            member: member.into(),
            criteria: criteria.trim().into(),
            turns: 0,
            cap: cap.max(1),
        }
    }

    pub(crate) fn begin(&mut self) -> String {
        self.turns = 1;
        self.turn_prompt()
    }

    pub(crate) fn after_turn(&mut self, outcome: Result<&str, &str>) -> Step {
        match outcome {
            Err(error) => Step::Failed(error.into()),
            Ok(reply) if reports_met(reply) => Step::Met,
            Ok(_) if self.turns >= self.cap => Step::CapReached,
            Ok(_) => {
                self.turns += 1;
                Step::Continue(self.turn_prompt())
            }
        }
    }

    pub(crate) fn status(&self) -> String {
        format!("turn {}/{} · {}", self.turns, self.cap, self.criteria)
    }

    fn turn_prompt(&self) -> String {
        format!(
            "Pinned goal (turn {turns} of at most {cap}): {criteria}\nWork toward this goal now. When the goal is fully met, end your reply with a line containing exactly {GOAL_MET}. Otherwise say what remains; you will get another turn.",
            turns = self.turns,
            cap = self.cap,
            criteria = self.criteria,
        )
    }
}

pub(crate) fn reports_met(reply: &str) -> bool {
    reply.lines().any(|line| line.trim() == GOAL_MET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_continues_until_the_agent_reports_goal_met() {
        let mut goal = Goal::new("Commander", "  tests pass  ", 10);
        let first = goal.begin();
        assert!(first.contains("turn 1 of at most 10): tests pass"));
        assert!(first.contains(GOAL_MET));
        let Step::Continue(second) = goal.after_turn(Ok("Fixed one test.")) else {
            panic!("expected another turn");
        };
        assert!(second.contains("turn 2 of at most 10"));
        assert_eq!(goal.status(), "turn 2/10 · tests pass");
        assert_eq!(goal.after_turn(Ok("All green.\nGOAL_MET\n")), Step::Met);
        assert_eq!(goal.turns, 2);
    }

    #[test]
    fn goal_stops_at_the_safety_cap_and_on_failure() {
        let mut goal = Goal::new("Scout", "ship it", 3);
        goal.begin();
        assert!(matches!(goal.after_turn(Ok("step")), Step::Continue(_)));
        assert!(matches!(goal.after_turn(Ok("step")), Step::Continue(_)));
        assert_eq!(goal.after_turn(Ok("step")), Step::CapReached);
        assert_eq!(goal.turns, 3);
        let mut failing = Goal::new("Scout", "ship it", 3);
        failing.begin();
        assert_eq!(
            failing.after_turn(Err("Provider exited")),
            Step::Failed("Provider exited".into())
        );
    }

    #[test]
    fn goal_met_must_be_its_own_line_and_cap_is_at_least_one() {
        assert!(!reports_met("Not GOAL_MET yet"));
        assert!(!reports_met("goal_met"));
        assert!(reports_met("done\n  GOAL_MET  "));
        let mut goal = Goal::new("Commander", "x", 0);
        assert_eq!(goal.cap, 1);
        goal.begin();
        assert_eq!(goal.after_turn(Ok("nope")), Step::CapReached);
    }
}
