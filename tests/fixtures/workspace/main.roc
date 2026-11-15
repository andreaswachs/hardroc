app [main!] {
	roc: "nightly-2026-11-09-9e85de7",
	util: "../package/main.roc",
}

import Color
import Src/Widget
import util.Util

## Build a widget, report its tint, and greet the user.
main! = |_| {
	widget = Widget.make("demo")
	greeting = Util.greet(widget.label)

	expect widget.label == "demo"
	expect greeting == "Hello, demo!"

	_ = Color.base.to_hex()

	Ok({})
}

## A multiline record whose field is a lambda.
settings = {
	label: "hardroc",
	greet: |name| "Hello, ${name}!",
	retries: 3,
}

record_greeting = settings.greet

expect record_greeting("Roc") == "Hello, Roc!"
