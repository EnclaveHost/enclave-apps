"""The loop, proven end to end against the stub: model asks for a tool, the
graph executes it client-side, the result goes back as role:"tool", and the
final answer quotes it. Plus the tools themselves, which never need a server.

Run:  python -m unittest discover -s tests -v   (from agent/)
"""

from __future__ import annotations

import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

from stub_server import start  # noqa: E402

from enclave_agent import Settings, build_agent, make_model, run_once  # noqa: E402
from enclave_agent.tools import calculator, extract_text  # noqa: E402


class CalculatorTests(unittest.TestCase):
    def test_arithmetic_is_exact(self):
        self.assertEqual(calculator.invoke({"expression": "6*7"}), "42")
        self.assertEqual(calculator.invoke({"expression": "(2**10) % 7"}), "2")
        self.assertEqual(calculator.invoke({"expression": "1/8"}), "0.125")

    def test_everything_but_arithmetic_is_refused(self):
        for evil in ("__import__('os')", "open('/etc/passwd')", "x+1", "1;2"):
            self.assertTrue(
                calculator.invoke({"expression": evil}).startswith("error:"), evil)

    def test_division_by_zero_is_an_answer_not_a_crash(self):
        self.assertEqual(calculator.invoke({"expression": "1/0"}),
                         "error: division by zero")


class ExtractorTests(unittest.TestCase):
    def test_scripts_die_and_blocks_break(self):
        html = ("<html><head><script>alert(1)</script><style>p{}</style></head>"
                "<body><h1>Title</h1><p>one</p><p>two</p></body></html>")
        text = extract_text(html)
        self.assertNotIn("alert", text)
        self.assertEqual(text.splitlines(), ["Title", "one", "two"])

    def test_truncation_is_marked(self):
        text = extract_text("<p>" + "a" * 10 + "</p>", limit=5)
        self.assertTrue(text.endswith("[truncated]"))


class AgentLoopTests(unittest.TestCase):
    """One user turn -> tool call -> tool result -> final answer."""

    @classmethod
    def setUpClass(cls):
        cls.server, base_url = start()
        settings = Settings(base_url=base_url, streaming=False)
        cls.settings = settings
        cls.agent = build_agent(make_model(settings))

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def test_the_loop_closes(self):
        messages = run_once(self.agent, "6*7", self.settings)
        # user, AI(tool_calls), tool, AI(final)
        self.assertEqual(len(messages), 4)
        self.assertEqual(messages[1].tool_calls[0]["name"], "calculator")
        self.assertEqual(messages[2].content, "42")
        self.assertIn("42", messages[3].content)

    def test_history_carries_across_turns(self):
        first = run_once(self.agent, "6*7", self.settings)
        second = run_once(self.agent, "10-3", self.settings, history=first)
        self.assertEqual(len(second), 8)
        self.assertEqual(second[-2].content, "7")


if __name__ == "__main__":
    unittest.main()
