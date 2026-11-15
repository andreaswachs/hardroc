app [main!] {
	roc: "nightly-2026-11-09-9e85de7",
}

import Color exposing [to_str]
import Other

main! = |_| {
	color = Color.red
	label = Color.to_str(color)
	exposed_label = to_str(color)
	other = Other.describe(color)
	text = Str.concat("a", "b")
	helper(color)
}

helper = |color| color

default_color : Color
default_color = Color.red
