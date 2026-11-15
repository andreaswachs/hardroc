## Greeting helpers exposed by the fixture package.
Util := {}.{

	## Greet someone by name.
	greet : Str -> Str
	greet = |name| "Hello, ${name}!"
}
