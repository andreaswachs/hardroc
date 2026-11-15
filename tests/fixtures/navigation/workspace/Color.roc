Color :: [Red, Green].{
	red : Color
	red = Red

	to_str : Color -> Str
	to_str = |color| match color {
		Red => "red"
		Green => "green"
	}
}
