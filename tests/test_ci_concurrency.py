"""Retained Horizon CI queue policy, with the shared offline YAML parser."""
from pathlib import Path
import sys
import unittest

ROOT = Path(__file__).resolve().parents[1]
# Use the same offline parser as the repository guard, not a runner package.
sys.path.insert(0, str(ROOT / "tools/ci/vendor/pyyaml-6.0.3.zip"))
import yaml


class ConcurrencyTests(unittest.TestCase):
    def test_ios_release_publications_share_a_queue_across_commits(self):
        workflow = yaml.safe_load((ROOT / ".github/workflows/ios.yml").read_text())
        # Per-SHA build groups cannot protect the read-latest/edit-latest sequence.
        self.assertEqual(workflow['jobs']['release'].get('concurrency'), {
            'group': 'horizon-ios-release', 'cancel-in-progress': False, 'queue': 'max',
        })

    def test_checks_replace_pull_requests_but_finish_the_running_main_check(self):
        path = ROOT / ".github/workflows/ci.yml"
        if not path.is_file():
            # The mobile fork retains its own build queue rather than desktop verify jobs.
            workflow = yaml.safe_load((ROOT / ".github/workflows/ios.yml").read_text())
            self.assertEqual(workflow["concurrency"], {
                "group": "ios-${{ github.sha }}", "cancel-in-progress": False,
            })
            return
        workflow = yaml.safe_load(path.read_text())
        self.assertEqual(workflow["concurrency"], {
            "group": "checks-${{ github.event.pull_request.number || "
                     "(github.event_name == 'push' && 'main') || github.sha }}",
            "cancel-in-progress": "${{ github.event_name == 'pull_request' }}",
        })


if __name__ == "__main__":
    unittest.main()
