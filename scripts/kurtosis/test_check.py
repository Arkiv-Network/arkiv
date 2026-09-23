"""Deterministic readiness checks without Docker, RPC servers, or real sleeps."""

import argparse
from contextlib import ExitStack
import io
import unittest
from unittest.mock import patch

import check


class NetworkReadinessTests(unittest.TestCase):
    def run_network(self, heights, *, start=(0, 0), mismatch=None, samples=3):
        args = argparse.Namespace(
            sequencer_rpc="sequencer",
            follower_rpc="follower",
            wait=len(heights) * check.POLL_INTERVAL,
            max_follower_lag=2,
            samples=samples,
        )
        elapsed = 0

        def sleep(seconds):
            nonlocal elapsed
            elapsed += seconds

        def block(url, number):
            suffix = "-different" if url == "follower" and number == mismatch else ""
            return {"number": hex(number), "hash": f"hash-{number}{suffix}"}

        self.stderr = io.StringIO()
        with ExitStack() as stack:
            stack.enter_context(patch.object(check, "DIAGNOSTIC_ENCLAVE", None))
            stack.enter_context(patch.object(check, "log"))
            stack.enter_context(patch("sys.stderr", self.stderr))
            stack.enter_context(patch.object(check.time, "monotonic", lambda: elapsed))
            stack.enter_context(patch.object(check.time, "sleep", sleep))
            stack.enter_context(
                patch.object(check, "check_rpc", side_effect=[("chain", n) for n in start])
            )
            stack.enter_context(
                patch.object(
                    check, "block_number", side_effect=[n for pair in heights for n in pair]
                )
            )
            stack.enter_context(patch.object(check, "block", side_effect=block))
            check.check_network(args)
        return elapsed

    def test_waits_for_genesis_and_initial_peer_discovery(self):
        heights = [(0, 0)] * 8 + [(1, 0), (2, 0), (4, 0), (5, 3), (6, 5), (7, 7)]
        self.assertEqual(self.run_network(heights), len(heights) * check.POLL_INTERVAL)

    def test_persistent_lag_fails_at_deadline_with_heights(self):
        with self.assertRaises(SystemExit):
            self.run_network([(1, 0), (4, 0), (5, 0), (6, 0)])
        self.assertIn("in 8s; sequencer=6, follower=0, lag=6, allowed=2", self.stderr.getvalue())

    def test_idle_sequencer_fails_even_if_follower_catches_up_to_old_head(self):
        with self.assertRaises(SystemExit):
            self.run_network([(5, 4), (5, 5), (5, 5)], start=(5, 3))
        self.assertIn("0/3 block samples", self.stderr.getvalue())

    def test_same_common_block_is_not_counted_twice(self):
        with self.assertRaises(SystemExit):
            self.run_network([(1, 1), (2, 1), (3, 1)])
        self.assertIn("1/3 block samples", self.stderr.getvalue())

    def test_follower_progress_counts_without_another_sequencer_block(self):
        self.run_network([(3, 1), (3, 2), (3, 3)])

    def test_hash_disagreement_fails_even_during_catch_up(self):
        with self.assertRaises(SystemExit):
            self.run_network([(5, 1)], mismatch=1)
        self.assertIn("nodes disagree at common height 1", self.stderr.getvalue())

    def test_excessive_lag_resets_healthy_sample_sequence(self):
        with self.assertRaises(SystemExit):
            self.run_network([(1, 1), (2, 2), (6, 2), (7, 7)])
        self.assertIn("1/3 block samples", self.stderr.getvalue())


if __name__ == "__main__":
    unittest.main()
