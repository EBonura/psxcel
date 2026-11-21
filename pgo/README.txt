nav-edit.pxtape: the speed window's input (psoxide-test.toml [[speed]] nav-edit). Recorded on the shipping
disc, digital pad, 2026-11-21:

  psoxide-test tape --disc build/ship/psxcel.cue --pad digital --steps 40000000000 --stop-at-poll 1300 \
    --out pgo/nav-edit.pxtape --press "<schedule>"

schedule (route ticks, tick:button:hold): 180:down:4 and 220:cross:4 open the Budget sample from the start
menu; 270..606 walk the cursor (right x3, down x5, left x2, up x3, every 28 ticks); 634 and 694 are an
L2+down and an L2+up page jump; 754:cross:4 opens the keyboard, 794..1028 move its highlight (up, right x2,
down, left x2, up, right, down x2), 1054:circle:4 closes it; 1094..1184 move the cursor again.
