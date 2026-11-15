## A small palette used by the workspace widgets.
Color := [Red, Green, Blue].{

	## Convert this color to a CSS hexadecimal string.
	to_hex : Color -> Str
	to_hex = |color| {
		match color {
			Red => "#ff0000"
			Green => "#00ff00"
			Blue => "#0000ff"
		}
	}

	## The color used by default widgets.
	base : Color
	base = Blue
}

## The palette name, used only inside this module.
palette_name = "fixture"

expect palette_name == "fixture"
