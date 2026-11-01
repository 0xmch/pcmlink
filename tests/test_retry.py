"""The run loop restarts after every failure and stops only on ctrl-c."""
import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
import pcmlink  # noqa: E402


def attempts(*outcomes):
    """An attempt() that plays back outcomes: a return code, or an exception to raise."""
    calls = []

    def attempt():
        outcome = outcomes[len(calls)]
        calls.append(outcome)
        if isinstance(outcome, BaseException):
            raise outcome
        return outcome

    return attempt, calls


@mock.patch.object(pcmlink.time, "sleep")
class WithRetry(unittest.TestCase):
    cfg = {"retry": True, "retry_delay": 5}

    def test_every_end_is_retried_until_ctrl_c(self, sleep):
        # An error (1), a clean end of stream (0) and a missing device (Retry)
        # are all failures of a live stream; only ctrl-c ends the loop.
        attempt, calls = attempts(1, 0, pcmlink.Retry("no audio sink"), KeyboardInterrupt())
        with self.assertRaises(KeyboardInterrupt):
            pcmlink._with_retry(self.cfg, attempt)
        self.assertEqual(len(calls), 4)
        self.assertEqual([c.args for c in sleep.call_args_list], [(5,)] * 3)

    def test_retry_off_returns_the_first_result(self, sleep):
        attempt, calls = attempts(pcmlink.Retry("pipeline failed to start"))
        code = pcmlink._with_retry({"retry": False, "retry_delay": 5}, attempt)
        self.assertEqual((code, len(calls)), (1, 1))
        sleep.assert_not_called()

    def test_delay_is_at_least_one_second(self, sleep):
        attempt, _ = attempts(1, KeyboardInterrupt())
        with self.assertRaises(KeyboardInterrupt):
            pcmlink._with_retry({"retry": True, "retry_delay": 0}, attempt)
        sleep.assert_called_once_with(1)


if __name__ == "__main__":
    unittest.main()
