"""Exercise actual Textual controls, layouts, and the development reload signal."""
import unittest


from textual.widgets import Input, ListView, Static

from app import AgentCommand, BACKDROP


class InterfaceTests(unittest.IsolatedAsyncioTestCase):
    async def test_keyboard_selection_pause_and_submit(self):
        app = AgentCommand()
        async with app.run_test(size=(150, 46)) as pilot:
            await pilot.click("#agents")
            await pilot.press("home", "down")
            self.assertEqual(app.query_one("#agents", ListView).index, 1)
            await pilot.press("ctrl+p")
            self.assertIn(1, app.paused)
            await pilot.press("ctrl+p")
            self.assertNotIn(1, app.paused)
            await pilot.press("ctrl+n")
            self.assertIs(app.focused, app.query_one("#command", Input))
            await pilot.press(*list("Review the UI"), "enter")
            self.assertIn("Review the UI", str(app.query_one("#mission", Static).render()))
            self.assertEqual(app.query_one("#command", Input).value, "")

    async def test_layout_at_terminal_sizes(self):
        for size in ((120, 40), (150, 46), (210, 54)):
            with self.subTest(size=size):
                app = AgentCommand()
                async with app.run_test(size=size) as pilot:
                    await pilot.pause()
                    banner = app.query_one("#wordmark")
                    self.assertLessEqual(max(map(len, BACKDROP.splitlines())), banner.content_size.width)
                    for selector in ("#main", "#command-row", "#status"):
                        widget = app.query_one(selector)
                        self.assertLessEqual(widget.region.bottom, size[1])

    async def test_source_change_exits_for_reload(self):
        app = AgentCommand()
        async with app.run_test(size=(150, 46)) as pilot:
            await pilot.pause()
            app._source_stamp = -1
            app.watch_source()
            await pilot.pause()
        self.assertEqual(app.return_value, "reload")

    async def test_style_watch_is_enabled(self):
        app = AgentCommand(watch_css=True)
        async with app.run_test(size=(150, 46)):
            self.assertIsNotNone(app.css_monitor)


if __name__ == "__main__":
    unittest.main()
