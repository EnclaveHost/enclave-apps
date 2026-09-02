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

from stub_notes import start as start_notes  # noqa: E402
from stub_server import start  # noqa: E402

from enclave_agent import Settings, build_agent, make_model, run_once  # noqa: E402
from enclave_agent.tools import (  # noqa: E402
    NotesClient, calculator, extract_text, make_notes_tools)


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


class NotebookToolTests(unittest.TestCase):
    """The six jot tools against a stub deployment: names travel percent-
    encoded, the key rides as a bearer, errors come back as text."""

    @classmethod
    def setUpClass(cls):
        cls.server, base_url, cls.notes = start_notes()
        cls.tools = {t.name: t for t in make_notes_tools(NotesClient(base_url, "stub-key"))}
        cls.bad = {t.name: t for t in make_notes_tools(NotesClient(base_url, "wrong"))}

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()

    def setUp(self):
        self.notes.clear()

    def test_write_read_list_search_delete(self):
        t = self.tools
        self.assertEqual(t["notes_list"].invoke({}), "(no notes)")
        self.assertEqual(t["notes_write"].invoke({"name": "projects/enclave.md",
                                                  "content": "# Enclave\nneedle here\n"}),
                         "saved projects/enclave.md (22 B)")
        self.assertEqual(self.notes["projects/enclave.md"], "# Enclave\nneedle here\n")
        self.assertEqual(t["notes_read"].invoke({"name": "projects/enclave.md"}),
                         "# Enclave\nneedle here\n")
        self.assertTrue(t["notes_list"].invoke({}).startswith("projects/enclave.md  (22 B"))
        self.assertEqual(t["notes_list"].invoke({"prefix": "zzz"}), "(no notes under zzz)")
        self.assertEqual(t["notes_search"].invoke({"query": "NEEDLE"}),
                         "projects/enclave.md:2: needle here")
        self.assertTrue(t["notes_search"].invoke({"query": "absent"}).startswith("no matches"))
        self.assertEqual(t["notes_delete"].invoke({"name": "projects/enclave.md"}),
                         "deleted projects/enclave.md")
        self.assertEqual(t["notes_read"].invoke({"name": "projects/enclave.md"}),
                         "error: no such note")

    def test_append_creates_then_extends(self):
        t = self.tools
        self.assertEqual(t["notes_append"].invoke({"name": "log.md", "content": "one"}),
                         "appended to log.md (now 4 B)")
        t["notes_append"].invoke({"name": "log.md", "content": "two"})
        self.assertEqual(self.notes["log.md"], "one\ntwo\n")

    def test_names_with_spaces_survive_the_url(self):
        t = self.tools
        t["notes_write"].invoke({"name": "meeting notes/2026-09-01.md", "content": "x"})
        self.assertIn("meeting notes/2026-09-01.md", self.notes)
        self.assertEqual(t["notes_read"].invoke({"name": "meeting notes/2026-09-01.md"}), "x")

    def test_wrong_key_is_an_error_string_not_an_exception(self):
        self.assertEqual(self.bad["notes_list"].invoke({}), "error: unauthorized")

    def test_unreachable_notebook_is_an_error_string(self):
        dead = {t.name: t for t in make_notes_tools(NotesClient("http://127.0.0.1:9", "k"))}
        self.assertTrue(dead["notes_list"].invoke({}).startswith("error: notebook unreachable"))


if __name__ == "__main__":
    unittest.main()
