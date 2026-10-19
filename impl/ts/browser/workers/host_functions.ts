// Application-owned registration code, using only the public worker entry point.
import { intValue, type EnvironmentBuilder, type HostFunctionSpec } from "jed-ts/browser/worker";

export async function configure(builder: EnvironmentBuilder): Promise<void> {
  // Registration genuinely suspends. Opening must await this before building the environment.
  await new Promise<void>((resolve) => setTimeout(resolve, 10));
  let calls = 0n;
  const add: HostFunctionSpec = {
    name: "app_add",
    argTypes: ["i64"],
    result: "i64",
    kernel: ([value]) => {
      if (value.kind !== "int") throw new Error("expected an integer argument");
      return intValue(value.int + 41n);
    },
  };
  builder.registerFunction(add);
  builder.registerFunction({
    name: "app_next",
    argTypes: [],
    result: "i64",
    kernel: () => intValue(++calls),
  });
}
