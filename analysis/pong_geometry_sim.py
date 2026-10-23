"""Exhaustive stationary-paddle geometry experiment; no game files modified.

Generalization of current contract: contact planes x=2 and width-3;
vertical walls y=0 and height-1; paddle radius 3; horizontal speed 2;
outgoing vy=clamp(contact_y-paddle_y,-2,2). Uniform angled opening serves
from x=width//2, y=2..height-3, vx=+/-1, vy=+/-1,+/-2.
Classifies exact repeated contact states, rather than using a rally timeout.
Horizontal trajectories are reported separately, not counted as angled cycles.
"""
import csv
from pathlib import Path


def fold(y, height):
    phase = y % (2 * (height - 1))
    return min(phase, 2 * (height - 1) - phase)


def experiment(width, height):
    cross_ticks = (width - 5 + 1) // 2
    totals = {group: dict(goal=0, horizontal=0, cycle=0, total=0)
              for group in ('all_positions', 'aligned', 'center')}
    max_cycle = 0
    for left in range(3, height - 3):
        for right in range(3, height - 3):
            cache = {}

            def classify(start):
                nonlocal max_cycle
                state, path, seen = start, [], {}
                while state not in cache and state not in seen:
                    y, target, vy = state
                    center = right if target == 1 else left
                    offset = y - center
                    if abs(offset) > 3:
                        cache[state] = 'goal'
                        break
                    outgoing = max(-2, min(2, offset))
                    if outgoing == 0 and left == right == y:
                        # Only an indefinitely horizontal rally is excluded.
                        cache[state] = 'horizontal'
                        break
                    seen[state] = len(path)
                    path.append(state)
                    state = (fold(y + outgoing * cross_ticks, height), 1-target, outgoing)
                if state in seen:
                    max_cycle = max(max_cycle, len(path) - seen[state])
                    result = 'cycle'
                else:
                    result = cache[state]
                for visited in path:
                    cache[visited] = result
                return result

            groups = ['all_positions']
            if left == right:
                groups.append('aligned')
            if left == right == height // 2:
                groups.append('center')
            for y in range(2, height - 2):
                for vx in (-1, 1):
                    distance = width - 3 - width // 2 if vx == 1 else width // 2 - 2
                    opening_ticks = (distance + 1) // 2
                    for vy in (-2, -1, 1, 2):
                        contact_y = fold(y + vy * opening_ticks, height)
                        result = classify((contact_y, int(vx == 1), vy))
                        for group in groups:
                            totals[group][result] += 1
                            totals[group]['total'] += 1
    row = dict(width=width, height=height, cross_ticks=cross_ticks,
               steep_travel=2*cross_ticks, reflection_period=2*(height-1), max_cycle_contacts=max_cycle)
    for group, counts in totals.items():
        row[group+'_cases'] = counts['total']
        for outcome in ('goal', 'horizontal', 'cycle'):
            row[group+'_'+outcome+'_pct'] = round(100*counts[outcome]/counts['total'], 4)
    return row


if __name__ == '__main__':
    dimensions = sorted({(w, 32) for w in range(48, 81)} | {(64, h) for h in range(26, 41)})
    rows = [experiment(w, h) for w, h in dimensions]
    output = Path(__file__).with_name('pong_geometry_results.csv')
    with output.open('w', newline='') as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]))
        writer.writeheader()
        writer.writerows(rows)
    print('Current:', next(row for row in rows if (row['width'], row['height']) == (64, 32)))
    print('Candidates, ranked by all-position angled-cycle rate:')
    for row in sorted(rows, key=lambda r: (r['all_positions_cycle_pct'], abs(r['width']-64)+abs(r['height']-32)))[:12]:
        print(row)
    print('Nearby widths:')
    for row in rows:
        if row['height'] == 32 and 58 <= row['width'] <= 72:
            print({key: row[key] for key in ('width','cross_ticks','all_positions_cycle_pct','aligned_cycle_pct','center_cycle_pct','all_positions_horizontal_pct')})
    print('Results:', output)
