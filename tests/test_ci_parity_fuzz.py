"""parity-fuzz.yml handles the vanilla data by parity-hosted.yml's rules: the steps that hold a secret are that file's,
line for line, and what it leaves out stays out (no other trigger, no secret or data in the build job, no cache
written)."""
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# Use the same offline parser as the repository guard, not a runner package.
sys.path.insert(0, str(ROOT / "tools/ci/vendor/pyyaml-6.0.3.zip"))
import yaml

FUZZ = ROOT / ".github/workflows/parity-fuzz.yml"
HOSTED = ROOT / ".github/workflows/parity-hosted.yml"
SECRET = re.compile(r"secrets\.[A-Z_]+")


def steps(workflow, job):
    return workflow["jobs"][job]["steps"]


def named(workflow, job, name):
    found = [step for step in steps(workflow, job) if step.get("name") == name]
    assert len(found) == 1, (job, name, len(found))
    return found[0]


class ParityFuzzWorkflowTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = FUZZ.read_text(encoding="utf-8")
        cls.fuzz = yaml.safe_load(cls.text)
        cls.hosted = yaml.safe_load(HOSTED.read_text(encoding="utf-8"))

    def test_a_hand_start_is_the_only_trigger_and_the_token_only_reads(self):
        # PyYAML reads the key `on` as True.
        self.assertEqual(list(self.fuzz[True]), ["workflow_dispatch"])
        self.assertEqual(self.fuzz["permissions"], {"contents": "read"})
        for name, job in self.fuzz["jobs"].items():
            self.assertNotIn("permissions", job, name)
        self.assertNotIn("pull_request", self.text)

    def test_the_plan_steps_are_hosted_paritys(self):
        for name in ("Access to the vanilla data", "The commit of the trusted scripts",
                     "The commit to test, on a branch of this repository"):
            self.assertEqual(named(self.fuzz, "plan", name), named(self.hosted, "plan", name), name)

    def test_the_fetch_step_is_hosted_paritys(self):
        for name in ("Folders for the data, outside the checkout", "Restore the encrypted dataset",
                     "Fetch the vanilla data"):
            self.assertEqual(named(self.fuzz, "fuzz", name), named(self.hosted, "run", name), name)

    def test_the_unseal_step_is_hosted_paritys_for_the_one_entry_this_job_reads(self):
        ours = named(self.fuzz, "fuzz", "Unseal the restored caches")
        theirs = named(self.hosted, "run", "Unseal the restored caches")
        loop = 'for entry in "prepared $PREPARED" "oracle $ORACLE_PREFIX" "build $BUILD_PREFIX"; do'
        self.assertEqual(theirs["run"].count(loop), 1)
        self.assertEqual(ours["run"], theirs["run"].replace(loop, 'for entry in "prepared $PREPARED"; do'))
        env = dict(theirs["env"])
        for key in ("ORACLE_PREFIX", "BUILD_PREFIX"):
            del env[key]
        self.assertEqual(ours["env"], env)
        self.assertEqual({key: value for key, value in ours.items() if key not in ("run", "env")},
                         {key: value for key, value in theirs.items() if key not in ("run", "env")})

    def test_only_the_fetch_and_unseal_steps_hold_a_secret(self):
        holders = {}
        for job in self.fuzz["jobs"]:
            for step in steps(self.fuzz, job):
                found = set(SECRET.findall(yaml.safe_dump(step)))
                if found:
                    holders[(job, step.get("name"))] = found
        key, passphrase = "secrets.VANILLA_DATA_DEPLOY_KEY", "secrets.VANILLA_DATA_PASSPHRASE"
        self.assertEqual(holders, {
            ("plan", "Access to the vanilla data"): {key, passphrase},  # only whether they are set
            ("fuzz", "Fetch the vanilla data"): {key, passphrase},
            ("fuzz", "Unseal the restored caches"): {passphrase},
        })
        # Nowhere outside a step either (a job's or the workflow's env would reach every step).
        self.assertEqual(len(SECRET.findall(self.text)), 5)
        self.assertNotIn("secrets: inherit", self.text)

    def test_nothing_of_the_tested_commit_runs_before_the_last_secret(self):
        fuzz = steps(self.fuzz, "fuzz")
        last_secret = max(i for i, step in enumerate(fuzz) if SECRET.search(yaml.safe_dump(step)))
        before = fuzz[:last_secret + 1]
        self.assertFalse([step for step in before if "checkout" in str(step.get("uses", ""))
                          or "download-artifact" in str(step.get("uses", ""))])
        # The workspace is still empty there: the first checkout comes after.
        self.assertIn("actions/checkout@", str(fuzz[last_secret + 1].get("uses", "")))

    def test_the_build_job_has_no_secret_no_data_and_no_cache(self):
        build = yaml.safe_dump(self.fuzz["jobs"]["build"])
        for word in ("secrets.", "VANILLA", "actions/cache", "download-artifact", "GH_TOKEN", "github.token"):
            self.assertNotIn(word, build)
        self.assertEqual(self.fuzz["jobs"]["build"]["needs"], "plan")

    def test_no_job_writes_a_cache(self):
        for job in self.fuzz["jobs"]:
            for step in steps(self.fuzz, job):
                uses = str(step.get("uses", ""))
                self.assertFalse(uses.startswith("actions/cache@") or uses.startswith("actions/cache/save@"), uses)
                self.assertNotRegex(step.get("run", ""), r"sealed-cache\.sh\"? seal\b")

    def test_every_action_is_pinned_to_a_commit(self):
        for job in self.fuzz["jobs"]:
            for step in steps(self.fuzz, job):
                if "uses" in step:
                    self.assertRegex(step["uses"], r"^[\w./-]+@[0-9a-f]{40}$")

    def test_inputs_reach_a_shell_only_as_variables(self):
        for job in self.fuzz["jobs"]:
            for step in steps(self.fuzz, job):
                self.assertNotIn("${{", step.get("run", ""), step.get("name"))


if __name__ == "__main__":
    unittest.main()
