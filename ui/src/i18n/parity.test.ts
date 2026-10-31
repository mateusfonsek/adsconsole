import pt from "./pt.json";
import en from "./en.json";

function keys(obj: Record<string, unknown>, prefix = ""): string[] {
  return Object.entries(obj)
    .flatMap(([k, v]) =>
      v && typeof v === "object" ? keys(v as Record<string, unknown>, `${prefix}${k}.`) : [`${prefix}${k}`],
    )
    .sort();
}

test("pt e en têm as mesmas chaves", () => {
  expect(keys(en)).toEqual(keys(pt));
});
