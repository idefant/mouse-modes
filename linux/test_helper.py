"""Pure engine and relay capture tests; no root or physical input needed."""
import types
import unittest
from unittest.mock import patch

import mouse_modes_helper as helper


class ModeTests(unittest.TestCase):
    def setUp(self):
        self.modes = helper.Modes()
        self.modes.bindings.update({name: {'keys': ['F' + str(index + 13)]}
                                   for index, name in enumerate(helper.ACTIONS)})

    def test_defaults_pass_through_and_settings_apply_while_held(self):
        modes = helper.Modes()
        modes.update({'F13'})
        self.assertEqual(modes.motion(20, -20), (20, -20, 0, 0))
        modes.bindings['slow'] = {'keys': ['F13']}
        modes.update({'F13'})
        self.assertEqual(modes.motion(20, -20), (5, -5, 0, 0))

    def test_fast_wins_and_release_restores(self):
        self.modes.update({'F13', 'F14'})
        self.assertEqual(self.modes.motion(3, -3), (6, -6, 0, 0))
        self.modes.update({'F13'})
        self.assertEqual(self.modes.motion(4, -4), (1, -1, 0, 0))
        self.modes.update(set())
        self.assertEqual(self.modes.motion(3, -3), (3, -3, 0, 0))

    def test_fraction_preserved_but_reset_between_modes(self):
        self.modes.update({'F13'})
        self.assertEqual(self.modes.motion(3, -3), (0, 0, 0, 0))
        self.assertEqual(self.modes.motion(1, -1), (1, -1, 0, 0))
        self.modes.motion(3, 3)
        self.modes.update(set())
        self.assertEqual(self.modes.motion(1, 1), (1, 1, 0, 0))

    def test_scroll_freezes_both_pointer_axes(self):
        self.modes.update({'F15'})
        self.assertEqual(self.modes.motion(45, 80), (0, 0, 2, 0))
        self.modes.update({'F16'})
        self.assertEqual(self.modes.motion(45, 80), (0, 0, 0, -4))

    def test_both_action_and_simultaneous_axes(self):
        for held in ({'F17'}, {'F15', 'F16'}):
            self.modes.update(held)
            self.assertEqual(self.modes.motion(-40, 60), (0, 0, -2, -3))

    def test_scroll_cap_and_accumulation_reset(self):
        self.modes.update({'F17'})
        self.assertEqual(self.modes.motion(1000, -1000), (0, 0, 20, 20))
        self.assertEqual(self.modes.motion(0, 0), (0, 0, 20, 20))
        self.modes.update(set())
        self.modes.update({'F17'})
        self.assertEqual(self.modes.motion(0, 0), (0, 0, 0, 0))

    def test_modifier_sides_and_normalization(self):
        self.assertFalse(helper.matches({'keys': ['ControlLeft', 'KeyA']}, {'ControlRight', 'KeyA'}))
        self.assertTrue(helper.matches({'keys': ['Control', 'KeyA']}, {'ControlRight', 'KeyA'}))
        self.assertTrue(helper.matches({'keys': ['Control', 'KeyA']}, {'ControlLeft', 'KeyA', 'ShiftRight'}))
        self.assertEqual(helper.normalize({'ControlLeft', 'ControlRight', 'AltLeft', 'KeyA'}),
                         ['AltLeft', 'Control', 'KeyA'])


class Virtual:
    def __init__(self):
        self.events = []

    def write(self, kind, code, value):
        self.events.append((kind, code, value))

    def write_event(self, event):
        self.events.append((event.type, event.code, event.value))

    def syn(self):
        pass


class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.relay = helper.Relay.__new__(helper.Relay)
        self.relay.e = types.SimpleNamespace(EV_SYN=0, EV_KEY=1, EV_REL=2, EV_MSC=4,
                                            SYN_DROPPED=3, SYN_REPORT=0)
        self.relay.names = {29: 'ControlLeft', 97: 'ControlRight', 30: 'KeyA', 1: 'Escape'}
        self.device = {'keyboard': True, 'mouse': False, 'held': set(),
                       'forwarded': set(), 'virtual': Virtual()}
        self.relay.devices = {'keyboard': self.device}
        self.relay.modes = helper.Modes()
        self.relay.capturing = self.relay.drain = False
        self.relay.capture_keys = set()
        self.relay.capture_blocked = False
        self.relay.capture_deadline = 0
        self.emitted = []
        self.patcher = patch.object(helper, 'emit', lambda event, value: self.emitted.append((event, value)))
        self.patcher.start()
        self.addCleanup(self.patcher.stop)

    def key(self, code, value):
        self.relay.event(self.device, types.SimpleNamespace(type=1, code=code, value=value))

    def test_capture_suppresses_down_repeat_and_release(self):
        self.relay.command({'command': 'capture'})
        for code, value in ((29, 1), (97, 1), (30, 1), (30, 2), (29, 0), (30, 0), (97, 0)):
            self.key(code, value)
        self.assertEqual(self.device['virtual'].events, [])
        self.assertIn(('hotkey-captured', {'keys': ['Control', 'KeyA']}), self.emitted)
        self.key(30, 1)
        self.key(30, 0)
        self.assertEqual(self.device['virtual'].events, [(1, 30, 1), (1, 30, 0)])

    def test_cancel_drains_all_held_keys_without_replaying(self):
        self.relay.command({'command': 'capture'})
        self.key(29, 1)
        self.key(30, 1)
        self.relay.command({'command': 'cancel'})
        self.key(30, 2)
        self.key(30, 0)
        self.key(29, 0)
        self.assertEqual(self.device['virtual'].events, [])
        self.assertFalse(self.relay.drain)
        self.assertFalse(any(event == 'hotkey-captured' for event, _ in self.emitted))

    def test_already_held_keys_are_released_then_excluded(self):
        self.key(29, 1)
        self.relay.command({'command': 'capture'})
        self.key(29, 0)
        self.key(30, 1)
        self.key(30, 0)
        self.assertIn(('hotkey-captured', {'keys': ['KeyA']}), self.emitted)
        self.assertEqual(self.device['virtual'].events, [(1, 29, 1), (1, 29, 0)])

    def test_overflow_fails_closed(self):
        with self.assertRaises(RuntimeError):
            self.relay.event(self.device, types.SimpleNamespace(type=0, code=3, value=0))


if __name__ == '__main__':
    unittest.main()
