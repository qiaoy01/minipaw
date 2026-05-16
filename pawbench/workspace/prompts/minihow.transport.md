1. When the task requires 'turn to face the bearing', EXEC robot_move_turn with the bearing angle (positive=clockwise). Never describe turning without executing it.
2. For transport tasks that list numbered steps, execute each step with an EXEC command in order, do not skip any step, and issue DONE only after the final step's results are visible.
