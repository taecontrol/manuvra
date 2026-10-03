#!/usr/bin/env python3
"""Verify the retained 390px CLI fixture capture on each native CI platform."""
import json
from pathlib import Path
import struct
import sys

directory = Path(sys.argv[1])
png = (directory / 'cli-viewport-390x844.png').read_bytes()
assert png[:8] == b'\x89PNG\r\n\x1a\n' and png[12:16] == b'IHDR'
assert struct.unpack('>II', png[16:24]) == (390, 844), 'CLI capture has the wrong dimensions'
provenance = json.loads((directory / 'cli-viewport-provenance.json').read_text())
assert provenance['viewport'] == {'width': 390, 'height': 844, 'initial_client_width': 390}
observation = json.loads((directory / 'cli-viewport-390x844-observation.json').read_text())
assert observation['viewport']['width'] == 390 and observation['viewport']['height'] == 844
facts = json.loads(next(element['value'] for element in observation['elements']
                        if element['name'] == 'Viewport facts'))
for name, expected in {'client_width': 390, 'inner_width': 390, 'scale': 1, 'device_scale': 1,
                       'wrapped': False, 'hover': True, 'fine_pointer': True, 'touch_points': 0}.items():
    assert facts[name] == expected, f'CLI capture fixture fact differs: {name}'
print('CLI viewport capture: 390x844, initial390, desktop pointer, no wrapping')
