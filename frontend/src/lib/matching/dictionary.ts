/**
 * Bilingual grocery dictionary for English/Spanish term matching.
 * Used for duplicate detection and category auto-assignment.
 */

// Category IDs matching the database seed (supabase/migrations/002_seed_categories.sql)
export const CATEGORY_IDS = {
  PRODUCE: "c1000000-0000-0000-0000-000000000001",
  MEAT_FISH: "c1000000-0000-0000-0000-000000000002",
  DAIRY: "c1000000-0000-0000-0000-000000000003",
  BAKERY: "c1000000-0000-0000-0000-000000000004",
  PANTRY: "c1000000-0000-0000-0000-000000000005",
  FROZEN: "c1000000-0000-0000-0000-000000000006",
  SNACKS: "c1000000-0000-0000-0000-000000000007",
  DRINKS: "c1000000-0000-0000-0000-000000000008",
  BABY: "c1000000-0000-0000-0000-000000000009",
  HOUSEHOLD: "c1000000-0000-0000-0000-000000000010",
  TOILETRIES: "c1000000-0000-0000-0000-000000000011",
} as const;

export type CategoryId = (typeof CATEGORY_IDS)[keyof typeof CATEGORY_IDS];

export interface DictionaryEntry {
  termEn: string;
  termEs: string;
  canonical: string;
  categoryId: CategoryId;
  variants?: string[];
}

/**
 * Bilingual dictionary of grocery terms.
 * Each entry includes English/Spanish translations, a canonical form,
 * the category ID, and optional variants for matching.
 */
export const DICTIONARY: readonly DictionaryEntry[] = [
  // ============================================
  // PRODUCE (Frutas y Verduras) - ~25 items
  // ============================================
  {
    termEn: "apple",
    termEs: "manzana",
    canonical: "apple",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["apples", "manzanas"],
  },
  {
    termEn: "banana",
    termEs: "platano",
    canonical: "banana",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["bananas", "platanos", "guineo", "guineos"],
  },
  {
    termEn: "orange",
    termEs: "naranja",
    canonical: "orange",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["oranges", "naranjas"],
  },
  {
    termEn: "lemon",
    termEs: "limon",
    canonical: "lemon",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["lemons", "limones"],
  },
  {
    termEn: "lime",
    termEs: "lima",
    canonical: "lime",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["limes", "limas"],
  },
  {
    termEn: "grape",
    termEs: "uva",
    canonical: "grape",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["grapes", "uvas"],
  },
  {
    termEn: "strawberry",
    termEs: "fresa",
    canonical: "strawberry",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["strawberries", "fresas", "frutilla", "frutillas"],
  },
  {
    termEn: "blueberry",
    termEs: "arandano",
    canonical: "blueberry",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["blueberries", "arandanos"],
  },
  {
    termEn: "watermelon",
    termEs: "sandia",
    canonical: "watermelon",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["sandias"],
  },
  {
    termEn: "melon",
    termEs: "melon",
    canonical: "melon",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["melons", "melones", "cantaloupe"],
  },
  {
    termEn: "pineapple",
    termEs: "pina",
    canonical: "pineapple",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["pineapples", "pinas", "anana"],
  },
  {
    termEn: "mango",
    termEs: "mango",
    canonical: "mango",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["mangos", "mangoes"],
  },
  {
    termEn: "peach",
    termEs: "durazno",
    canonical: "peach",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["peaches", "duraznos", "melocoton", "melocotones"],
  },
  {
    termEn: "pear",
    termEs: "pera",
    canonical: "pear",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["pears", "peras"],
  },
  {
    termEn: "avocado",
    termEs: "aguacate",
    canonical: "avocado",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["avocados", "aguacates", "palta", "paltas"],
  },
  {
    termEn: "tomato",
    termEs: "tomate",
    canonical: "tomato",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["tomatoes", "tomates", "jitomate", "jitomates"],
  },
  {
    termEn: "potato",
    termEs: "papa",
    canonical: "potato",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["potatoes", "papas", "patata", "patatas"],
  },
  {
    termEn: "onion",
    termEs: "cebolla",
    canonical: "onion",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["onions", "cebollas"],
  },
  {
    termEn: "garlic",
    termEs: "ajo",
    canonical: "garlic",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["ajos"],
  },
  {
    termEn: "carrot",
    termEs: "zanahoria",
    canonical: "carrot",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["carrots", "zanahorias"],
  },
  {
    termEn: "broccoli",
    termEs: "brocoli",
    canonical: "broccoli",
    categoryId: CATEGORY_IDS.PRODUCE,
  },
  {
    termEn: "lettuce",
    termEs: "lechuga",
    canonical: "lettuce",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["lechugas"],
  },
  {
    termEn: "spinach",
    termEs: "espinaca",
    canonical: "spinach",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["espinacas"],
  },
  {
    termEn: "cucumber",
    termEs: "pepino",
    canonical: "cucumber",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["cucumbers", "pepinos"],
  },
  {
    termEn: "bell pepper",
    termEs: "pimiento",
    canonical: "bell pepper",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["bell peppers", "pimientos", "chile morron", "chiles morrones"],
  },
  {
    termEn: "celery",
    termEs: "apio",
    canonical: "celery",
    categoryId: CATEGORY_IDS.PRODUCE,
  },
  {
    termEn: "corn",
    termEs: "maiz",
    canonical: "corn",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["elote", "elotes", "choclo"],
  },
  {
    termEn: "mushroom",
    termEs: "champiñon",
    canonical: "mushroom",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: [
      "mushrooms",
      "champiñones",
      "hongo",
      "hongos",
      "champinon",
      "champinones",
    ],
  },
  {
    termEn: "zucchini",
    termEs: "calabacin",
    canonical: "zucchini",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["zucchinis", "calabacines", "calabaza italiana"],
  },
  {
    termEn: "eggplant",
    termEs: "berenjena",
    canonical: "eggplant",
    categoryId: CATEGORY_IDS.PRODUCE,
    variants: ["eggplants", "berenjenas"],
  },

  // ============================================
  // MEAT & FISH (Carne y Pescado) - ~20 items
  // ============================================
  {
    termEn: "chicken",
    termEs: "pollo",
    canonical: "chicken",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "chicken breast",
    termEs: "pechuga de pollo",
    canonical: "chicken breast",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["chicken breasts", "pechugas de pollo", "pechuga"],
  },
  {
    termEn: "chicken thigh",
    termEs: "muslo de pollo",
    canonical: "chicken thigh",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["chicken thighs", "muslos de pollo"],
  },
  {
    termEn: "ground beef",
    termEs: "carne molida",
    canonical: "ground beef",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["beef mince", "minced beef", "carne picada"],
  },
  {
    termEn: "beef steak",
    termEs: "bistec",
    canonical: "beef steak",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["steak", "steaks", "bistecs", "filete", "filetes"],
  },
  {
    termEn: "pork",
    termEs: "cerdo",
    canonical: "pork",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["puerco", "cochino"],
  },
  {
    termEn: "pork chop",
    termEs: "chuleta de cerdo",
    canonical: "pork chop",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["pork chops", "chuletas de cerdo", "chuleta"],
  },
  {
    termEn: "bacon",
    termEs: "tocino",
    canonical: "bacon",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["panceta", "tocineta"],
  },
  {
    termEn: "ham",
    termEs: "jamon",
    canonical: "ham",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "sausage",
    termEs: "salchicha",
    canonical: "sausage",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["sausages", "salchichas", "chorizo", "chorizos"],
  },
  {
    termEn: "turkey",
    termEs: "pavo",
    canonical: "turkey",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "lamb",
    termEs: "cordero",
    canonical: "lamb",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["borrego"],
  },
  {
    termEn: "fish",
    termEs: "pescado",
    canonical: "fish",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "salmon",
    termEs: "salmon",
    canonical: "salmon",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "tuna",
    termEs: "atun",
    canonical: "tuna",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "shrimp",
    termEs: "camaron",
    canonical: "shrimp",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["shrimps", "camarones", "gamba", "gambas"],
  },
  {
    termEn: "cod",
    termEs: "bacalao",
    canonical: "cod",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "tilapia",
    termEs: "tilapia",
    canonical: "tilapia",
    categoryId: CATEGORY_IDS.MEAT_FISH,
  },
  {
    termEn: "crab",
    termEs: "cangrejo",
    canonical: "crab",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["crabs", "cangrejos", "jaiba"],
  },
  {
    termEn: "lobster",
    termEs: "langosta",
    canonical: "lobster",
    categoryId: CATEGORY_IDS.MEAT_FISH,
    variants: ["lobsters", "langostas"],
  },

  // ============================================
  // DAIRY (Lacteos) - ~20 items
  // ============================================
  {
    termEn: "milk",
    termEs: "leche",
    canonical: "milk",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "whole milk",
    termEs: "leche entera",
    canonical: "whole milk",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "skim milk",
    termEs: "leche descremada",
    canonical: "skim milk",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["nonfat milk", "leche desnatada"],
  },
  {
    termEn: "butter",
    termEs: "mantequilla",
    canonical: "butter",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["manteca"],
  },
  {
    termEn: "cheese",
    termEs: "queso",
    canonical: "cheese",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["cheeses", "quesos"],
  },
  {
    termEn: "cheddar cheese",
    termEs: "queso cheddar",
    canonical: "cheddar cheese",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["cheddar"],
  },
  {
    termEn: "mozzarella",
    termEs: "mozzarella",
    canonical: "mozzarella",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["queso mozzarella"],
  },
  {
    termEn: "cream cheese",
    termEs: "queso crema",
    canonical: "cream cheese",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "parmesan",
    termEs: "parmesano",
    canonical: "parmesan",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["queso parmesano"],
  },
  {
    termEn: "yogurt",
    termEs: "yogur",
    canonical: "yogurt",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["yoghurt", "yogures"],
  },
  {
    termEn: "greek yogurt",
    termEs: "yogur griego",
    canonical: "greek yogurt",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "eggs",
    termEs: "huevos",
    canonical: "eggs",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["egg", "huevo"],
  },
  {
    termEn: "sour cream",
    termEs: "crema agria",
    canonical: "sour cream",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["crema acida", "nata agria"],
  },
  {
    termEn: "heavy cream",
    termEs: "crema de leche",
    canonical: "heavy cream",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["whipping cream", "nata para montar", "crema para batir"],
  },
  {
    termEn: "half and half",
    termEs: "media crema",
    canonical: "half and half",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "cottage cheese",
    termEs: "requeson",
    canonical: "cottage cheese",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "ricotta",
    termEs: "ricotta",
    canonical: "ricotta",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["queso ricotta"],
  },
  {
    termEn: "oat milk",
    termEs: "leche de avena",
    canonical: "oat milk",
    categoryId: CATEGORY_IDS.DAIRY,
  },
  {
    termEn: "almond milk",
    termEs: "leche de almendra",
    canonical: "almond milk",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["leche de almendras"],
  },
  {
    termEn: "soy milk",
    termEs: "leche de soya",
    canonical: "soy milk",
    categoryId: CATEGORY_IDS.DAIRY,
    variants: ["leche de soja"],
  },

  // ============================================
  // BAKERY (Panaderia) - ~18 items
  // ============================================
  {
    termEn: "bread",
    termEs: "pan",
    canonical: "bread",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["breads", "panes"],
  },
  {
    termEn: "white bread",
    termEs: "pan blanco",
    canonical: "white bread",
    categoryId: CATEGORY_IDS.BAKERY,
  },
  {
    termEn: "whole wheat bread",
    termEs: "pan integral",
    canonical: "whole wheat bread",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["wheat bread", "pan de trigo integral"],
  },
  {
    termEn: "baguette",
    termEs: "baguette",
    canonical: "baguette",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["baguettes", "pan frances"],
  },
  {
    termEn: "roll",
    termEs: "bolillo",
    canonical: "roll",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["rolls", "bolillos", "panecillo", "panecillos"],
  },
  {
    termEn: "tortilla",
    termEs: "tortilla",
    canonical: "tortilla",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["tortillas"],
  },
  {
    termEn: "flour tortilla",
    termEs: "tortilla de harina",
    canonical: "flour tortilla",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["flour tortillas", "tortillas de harina"],
  },
  {
    termEn: "corn tortilla",
    termEs: "tortilla de maiz",
    canonical: "corn tortilla",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["corn tortillas", "tortillas de maiz"],
  },
  {
    termEn: "croissant",
    termEs: "croissant",
    canonical: "croissant",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["croissants", "cuerno", "cuernos", "medialuna"],
  },
  {
    termEn: "muffin",
    termEs: "muffin",
    canonical: "muffin",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["muffins", "panque", "panques"],
  },
  {
    termEn: "donut",
    termEs: "dona",
    canonical: "donut",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["donuts", "doughnut", "doughnuts", "donas", "rosquilla"],
  },
  {
    termEn: "bagel",
    termEs: "bagel",
    canonical: "bagel",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["bagels"],
  },
  {
    termEn: "pita bread",
    termEs: "pan pita",
    canonical: "pita bread",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["pita", "pitas"],
  },
  {
    termEn: "cake",
    termEs: "pastel",
    canonical: "cake",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["cakes", "pasteles", "torta", "bizcocho"],
  },
  {
    termEn: "pie",
    termEs: "pay",
    canonical: "pie",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["pies", "tarta", "tartas"],
  },
  {
    termEn: "cookies",
    termEs: "galletas",
    canonical: "cookies",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["cookie", "galleta"],
  },
  {
    termEn: "brownie",
    termEs: "brownie",
    canonical: "brownie",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["brownies"],
  },
  {
    termEn: "cupcake",
    termEs: "cupcake",
    canonical: "cupcake",
    categoryId: CATEGORY_IDS.BAKERY,
    variants: ["cupcakes", "panquecito", "pastelito"],
  },

  // ============================================
  // PANTRY (Despensa) - ~30 items
  // ============================================
  {
    termEn: "rice",
    termEs: "arroz",
    canonical: "rice",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "white rice",
    termEs: "arroz blanco",
    canonical: "white rice",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "brown rice",
    termEs: "arroz integral",
    canonical: "brown rice",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "pasta",
    termEs: "pasta",
    canonical: "pasta",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["pastas", "fideos"],
  },
  {
    termEn: "spaghetti",
    termEs: "espagueti",
    canonical: "spaghetti",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["espaguetis"],
  },
  {
    termEn: "macaroni",
    termEs: "macarrones",
    canonical: "macaroni",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["coditos"],
  },
  {
    termEn: "flour",
    termEs: "harina",
    canonical: "flour",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "sugar",
    termEs: "azucar",
    canonical: "sugar",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "brown sugar",
    termEs: "azucar morena",
    canonical: "brown sugar",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["piloncillo", "panela"],
  },
  {
    termEn: "salt",
    termEs: "sal",
    canonical: "salt",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "pepper",
    termEs: "pimienta",
    canonical: "pepper",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["black pepper", "pimienta negra"],
  },
  {
    termEn: "olive oil",
    termEs: "aceite de oliva",
    canonical: "olive oil",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "vegetable oil",
    termEs: "aceite vegetal",
    canonical: "vegetable oil",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["cooking oil", "aceite de cocina"],
  },
  {
    termEn: "vinegar",
    termEs: "vinagre",
    canonical: "vinegar",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "soy sauce",
    termEs: "salsa de soya",
    canonical: "soy sauce",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["salsa de soja"],
  },
  {
    termEn: "ketchup",
    termEs: "ketchup",
    canonical: "ketchup",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["catsup", "salsa catsup"],
  },
  {
    termEn: "mustard",
    termEs: "mostaza",
    canonical: "mustard",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "mayonnaise",
    termEs: "mayonesa",
    canonical: "mayonnaise",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["mayo"],
  },
  {
    termEn: "hot sauce",
    termEs: "salsa picante",
    canonical: "hot sauce",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["chile sauce"],
  },
  {
    termEn: "honey",
    termEs: "miel",
    canonical: "honey",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "peanut butter",
    termEs: "crema de cacahuate",
    canonical: "peanut butter",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["mantequilla de mani", "crema de mani"],
  },
  {
    termEn: "jam",
    termEs: "mermelada",
    canonical: "jam",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["jelly", "jalea"],
  },
  {
    termEn: "canned beans",
    termEs: "frijoles enlatados",
    canonical: "canned beans",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["beans", "frijoles"],
  },
  {
    termEn: "black beans",
    termEs: "frijoles negros",
    canonical: "black beans",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "canned tomatoes",
    termEs: "tomates enlatados",
    canonical: "canned tomatoes",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["diced tomatoes", "jitomates enlatados"],
  },
  {
    termEn: "tomato sauce",
    termEs: "salsa de tomate",
    canonical: "tomato sauce",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["pure de tomate"],
  },
  {
    termEn: "chicken broth",
    termEs: "caldo de pollo",
    canonical: "chicken broth",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["chicken stock", "consomé de pollo"],
  },
  {
    termEn: "cereal",
    termEs: "cereal",
    canonical: "cereal",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["cereals", "cereales"],
  },
  {
    termEn: "oatmeal",
    termEs: "avena",
    canonical: "oatmeal",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["oats", "hojuelas de avena"],
  },
  {
    termEn: "coffee",
    termEs: "cafe",
    canonical: "coffee",
    categoryId: CATEGORY_IDS.PANTRY,
  },
  {
    termEn: "tea",
    termEs: "te",
    canonical: "tea",
    categoryId: CATEGORY_IDS.PANTRY,
    variants: ["teas", "tes"],
  },

  // ============================================
  // FROZEN (Congelados) - ~18 items
  // ============================================
  {
    termEn: "ice cream",
    termEs: "helado",
    canonical: "ice cream",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["helados", "nieve"],
  },
  {
    termEn: "frozen pizza",
    termEs: "pizza congelada",
    canonical: "frozen pizza",
    categoryId: CATEGORY_IDS.FROZEN,
  },
  {
    termEn: "frozen vegetables",
    termEs: "verduras congeladas",
    canonical: "frozen vegetables",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["vegetales congelados"],
  },
  {
    termEn: "frozen fruit",
    termEs: "fruta congelada",
    canonical: "frozen fruit",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["frutas congeladas"],
  },
  {
    termEn: "frozen peas",
    termEs: "chicharos congelados",
    canonical: "frozen peas",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["peas", "chicharos", "guisantes congelados"],
  },
  {
    termEn: "frozen corn",
    termEs: "maiz congelado",
    canonical: "frozen corn",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["elote congelado"],
  },
  {
    termEn: "frozen chicken",
    termEs: "pollo congelado",
    canonical: "frozen chicken",
    categoryId: CATEGORY_IDS.FROZEN,
  },
  {
    termEn: "frozen fish",
    termEs: "pescado congelado",
    canonical: "frozen fish",
    categoryId: CATEGORY_IDS.FROZEN,
  },
  {
    termEn: "frozen shrimp",
    termEs: "camarones congelados",
    canonical: "frozen shrimp",
    categoryId: CATEGORY_IDS.FROZEN,
  },
  {
    termEn: "frozen fries",
    termEs: "papas congeladas",
    canonical: "frozen fries",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["french fries", "papas fritas congeladas"],
  },
  {
    termEn: "frozen waffles",
    termEs: "waffles congelados",
    canonical: "frozen waffles",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["waffles"],
  },
  {
    termEn: "ice",
    termEs: "hielo",
    canonical: "ice",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["hielos"],
  },
  {
    termEn: "frozen burrito",
    termEs: "burrito congelado",
    canonical: "frozen burrito",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["frozen burritos", "burritos congelados"],
  },
  {
    termEn: "frozen lasagna",
    termEs: "lasaña congelada",
    canonical: "frozen lasagna",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["lasagna"],
  },
  {
    termEn: "frozen dinner",
    termEs: "comida congelada",
    canonical: "frozen dinner",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["tv dinner", "cena congelada"],
  },
  {
    termEn: "popsicle",
    termEs: "paleta",
    canonical: "popsicle",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["popsicles", "paletas", "ice pop"],
  },
  {
    termEn: "frozen berries",
    termEs: "moras congeladas",
    canonical: "frozen berries",
    categoryId: CATEGORY_IDS.FROZEN,
    variants: ["berries congeladas", "frutos rojos congelados"],
  },
  {
    termEn: "sorbet",
    termEs: "sorbete",
    canonical: "sorbet",
    categoryId: CATEGORY_IDS.FROZEN,
  },

  // ============================================
  // SNACKS (Snacks) - ~18 items
  // ============================================
  {
    termEn: "chips",
    termEs: "papas fritas",
    canonical: "chips",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["potato chips", "papitas", "frituras"],
  },
  {
    termEn: "tortilla chips",
    termEs: "totopos",
    canonical: "tortilla chips",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["nachos", "chips de tortilla"],
  },
  {
    termEn: "popcorn",
    termEs: "palomitas",
    canonical: "popcorn",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["palomitas de maiz", "pochoclo", "cotufas"],
  },
  {
    termEn: "pretzels",
    termEs: "pretzels",
    canonical: "pretzels",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["pretzel"],
  },
  {
    termEn: "crackers",
    termEs: "galletas saladas",
    canonical: "crackers",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["cracker", "galletas de soda"],
  },
  {
    termEn: "nuts",
    termEs: "nueces",
    canonical: "nuts",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["mixed nuts", "nueces mixtas", "frutos secos"],
  },
  {
    termEn: "peanuts",
    termEs: "cacahuates",
    canonical: "peanuts",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["peanut", "cacahuate", "mani", "manies"],
  },
  {
    termEn: "almonds",
    termEs: "almendras",
    canonical: "almonds",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["almond", "almendra"],
  },
  {
    termEn: "granola bar",
    termEs: "barra de granola",
    canonical: "granola bar",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["granola bars", "barras de granola"],
  },
  {
    termEn: "chocolate",
    termEs: "chocolate",
    canonical: "chocolate",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["chocolates"],
  },
  {
    termEn: "candy",
    termEs: "dulces",
    canonical: "candy",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["candies", "dulce", "caramelos", "golosinas"],
  },
  {
    termEn: "gum",
    termEs: "chicle",
    canonical: "gum",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["chewing gum", "chicles"],
  },
  {
    termEn: "dried fruit",
    termEs: "fruta seca",
    canonical: "dried fruit",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["frutas secas", "fruta deshidratada"],
  },
  {
    termEn: "raisins",
    termEs: "pasas",
    canonical: "raisins",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["raisin", "pasa", "pasitas", "uvas pasas"],
  },
  {
    termEn: "trail mix",
    termEs: "mezcla de frutos secos",
    canonical: "trail mix",
    categoryId: CATEGORY_IDS.SNACKS,
  },
  {
    termEn: "beef jerky",
    termEs: "cecina",
    canonical: "beef jerky",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["jerky", "carne seca"],
  },
  {
    termEn: "cheese puffs",
    termEs: "cheetos",
    canonical: "cheese puffs",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["cheese balls", "bolitas de queso"],
  },
  {
    termEn: "rice cakes",
    termEs: "tortitas de arroz",
    canonical: "rice cakes",
    categoryId: CATEGORY_IDS.SNACKS,
    variants: ["rice cake", "galletas de arroz"],
  },

  // ============================================
  // DRINKS (Bebidas) - ~18 items
  // ============================================
  {
    termEn: "water",
    termEs: "agua",
    canonical: "water",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["bottled water", "agua embotellada"],
  },
  {
    termEn: "sparkling water",
    termEs: "agua mineral",
    canonical: "sparkling water",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["seltzer", "agua con gas", "agua gasificada"],
  },
  {
    termEn: "juice",
    termEs: "jugo",
    canonical: "juice",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["juices", "jugos", "zumo", "zumos"],
  },
  {
    termEn: "orange juice",
    termEs: "jugo de naranja",
    canonical: "orange juice",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["oj", "zumo de naranja"],
  },
  {
    termEn: "apple juice",
    termEs: "jugo de manzana",
    canonical: "apple juice",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["zumo de manzana"],
  },
  {
    termEn: "soda",
    termEs: "refresco",
    canonical: "soda",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["soft drink", "pop", "refrescos", "gaseosa"],
  },
  {
    termEn: "cola",
    termEs: "cola",
    canonical: "cola",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["coke", "coca cola", "pepsi"],
  },
  {
    termEn: "lemonade",
    termEs: "limonada",
    canonical: "lemonade",
    categoryId: CATEGORY_IDS.DRINKS,
  },
  {
    termEn: "iced tea",
    termEs: "te helado",
    canonical: "iced tea",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["ice tea", "te frio"],
  },
  {
    termEn: "energy drink",
    termEs: "bebida energetica",
    canonical: "energy drink",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["energy drinks", "bebidas energeticas"],
  },
  {
    termEn: "sports drink",
    termEs: "bebida deportiva",
    canonical: "sports drink",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["sports drinks", "bebida isotonica", "gatorade"],
  },
  {
    termEn: "beer",
    termEs: "cerveza",
    canonical: "beer",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["beers", "cervezas"],
  },
  {
    termEn: "wine",
    termEs: "vino",
    canonical: "wine",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["wines", "vinos"],
  },
  {
    termEn: "red wine",
    termEs: "vino tinto",
    canonical: "red wine",
    categoryId: CATEGORY_IDS.DRINKS,
  },
  {
    termEn: "white wine",
    termEs: "vino blanco",
    canonical: "white wine",
    categoryId: CATEGORY_IDS.DRINKS,
  },
  {
    termEn: "coconut water",
    termEs: "agua de coco",
    canonical: "coconut water",
    categoryId: CATEGORY_IDS.DRINKS,
  },
  {
    termEn: "smoothie",
    termEs: "batido",
    canonical: "smoothie",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["smoothies", "batidos", "licuado"],
  },
  {
    termEn: "hot chocolate",
    termEs: "chocolate caliente",
    canonical: "hot chocolate",
    categoryId: CATEGORY_IDS.DRINKS,
    variants: ["cocoa", "cacao"],
  },

  // ============================================
  // BABY (Bebe) - ~14 items
  // ============================================
  {
    termEn: "diapers",
    termEs: "pañales",
    canonical: "diapers",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["diaper", "pañal", "panales", "panal"],
  },
  {
    termEn: "baby wipes",
    termEs: "toallitas para bebe",
    canonical: "baby wipes",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["wipes", "toallitas humedas", "toallitas"],
  },
  {
    termEn: "baby formula",
    termEs: "formula para bebe",
    canonical: "baby formula",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["formula", "leche de formula", "leche para bebe"],
  },
  {
    termEn: "baby food",
    termEs: "comida para bebe",
    canonical: "baby food",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["papilla", "papillas", "pure para bebe"],
  },
  {
    termEn: "baby cereal",
    termEs: "cereal para bebe",
    canonical: "baby cereal",
    categoryId: CATEGORY_IDS.BABY,
  },
  {
    termEn: "baby bottle",
    termEs: "biberon",
    canonical: "baby bottle",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["baby bottles", "biberones", "mamila", "mamadera"],
  },
  {
    termEn: "pacifier",
    termEs: "chupete",
    canonical: "pacifier",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["pacifiers", "chupetes", "chupon", "chupones"],
  },
  {
    termEn: "baby lotion",
    termEs: "locion para bebe",
    canonical: "baby lotion",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["crema para bebe"],
  },
  {
    termEn: "baby shampoo",
    termEs: "champu para bebe",
    canonical: "baby shampoo",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["shampoo para bebe"],
  },
  {
    termEn: "baby powder",
    termEs: "talco para bebe",
    canonical: "baby powder",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["talco", "polvo para bebe"],
  },
  {
    termEn: "diaper cream",
    termEs: "crema para rozaduras",
    canonical: "diaper cream",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["diaper rash cream", "crema antipañalitis"],
  },
  {
    termEn: "baby oil",
    termEs: "aceite para bebe",
    canonical: "baby oil",
    categoryId: CATEGORY_IDS.BABY,
  },
  {
    termEn: "teething ring",
    termEs: "mordedor",
    canonical: "teething ring",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["teether", "teethers", "mordedores"],
  },
  {
    termEn: "sippy cup",
    termEs: "vaso entrenador",
    canonical: "sippy cup",
    categoryId: CATEGORY_IDS.BABY,
    variants: ["sippy cups", "vasos entrenadores", "vaso para bebe"],
  },

  // ============================================
  // HOUSEHOLD (Hogar y Limpieza) - ~20 items
  // ============================================
  {
    termEn: "paper towels",
    termEs: "toallas de papel",
    canonical: "paper towels",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["paper towel", "servitoallas", "papel de cocina"],
  },
  {
    termEn: "toilet paper",
    termEs: "papel higienico",
    canonical: "toilet paper",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["toilet tissue", "papel de baño"],
  },
  {
    termEn: "tissues",
    termEs: "pañuelos desechables",
    canonical: "tissues",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["tissue", "kleenex", "pañuelos de papel"],
  },
  {
    termEn: "trash bags",
    termEs: "bolsas de basura",
    canonical: "trash bags",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["garbage bags", "bolsas para basura"],
  },
  {
    termEn: "dish soap",
    termEs: "jabon para trastes",
    canonical: "dish soap",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["dishwashing liquid", "lavatrastes", "detergente para platos"],
  },
  {
    termEn: "laundry detergent",
    termEs: "detergente para ropa",
    canonical: "laundry detergent",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["laundry soap", "detergente", "jabon para ropa"],
  },
  {
    termEn: "fabric softener",
    termEs: "suavizante",
    canonical: "fabric softener",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["suavizante de ropa", "suavitel"],
  },
  {
    termEn: "bleach",
    termEs: "cloro",
    canonical: "bleach",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["blanqueador", "lejia"],
  },
  {
    termEn: "all purpose cleaner",
    termEs: "limpiador multiusos",
    canonical: "all purpose cleaner",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["multi purpose cleaner", "limpiador"],
  },
  {
    termEn: "glass cleaner",
    termEs: "limpiador de vidrios",
    canonical: "glass cleaner",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["window cleaner", "limpiavidrios"],
  },
  {
    termEn: "sponge",
    termEs: "esponja",
    canonical: "sponge",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["sponges", "esponjas"],
  },
  {
    termEn: "aluminum foil",
    termEs: "papel aluminio",
    canonical: "aluminum foil",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["foil", "aluminio", "papel de aluminio"],
  },
  {
    termEn: "plastic wrap",
    termEs: "plastico adherente",
    canonical: "plastic wrap",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["cling wrap", "saran wrap", "papel film"],
  },
  {
    termEn: "ziplock bags",
    termEs: "bolsas con cierre",
    canonical: "ziplock bags",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["ziploc", "bolsas hermeticas", "bolsas ziploc"],
  },
  {
    termEn: "batteries",
    termEs: "pilas",
    canonical: "batteries",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["battery", "pila", "baterias"],
  },
  {
    termEn: "light bulb",
    termEs: "foco",
    canonical: "light bulb",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["light bulbs", "bulb", "focos", "bombilla", "bombillas"],
  },
  {
    termEn: "candles",
    termEs: "velas",
    canonical: "candles",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["candle", "vela"],
  },
  {
    termEn: "matches",
    termEs: "cerillos",
    canonical: "matches",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["match", "cerillo", "fosforos"],
  },
  {
    termEn: "broom",
    termEs: "escoba",
    canonical: "broom",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["escobas"],
  },
  {
    termEn: "mop",
    termEs: "trapeador",
    canonical: "mop",
    categoryId: CATEGORY_IDS.HOUSEHOLD,
    variants: ["mops", "trapeadores", "fregona"],
  },

  // ============================================
  // TOILETRIES (Higiene Personal) - ~18 items
  // ============================================
  {
    termEn: "shampoo",
    termEs: "champu",
    canonical: "shampoo",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["shampoos", "champus"],
  },
  {
    termEn: "conditioner",
    termEs: "acondicionador",
    canonical: "conditioner",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["hair conditioner", "acondicionadores"],
  },
  {
    termEn: "body wash",
    termEs: "jabon liquido corporal",
    canonical: "body wash",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["shower gel", "gel de ducha", "gel de baño"],
  },
  {
    termEn: "soap",
    termEs: "jabon",
    canonical: "soap",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["soaps", "jabones", "bar soap", "jabon en barra"],
  },
  {
    termEn: "toothpaste",
    termEs: "pasta de dientes",
    canonical: "toothpaste",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["pasta dental", "crema dental"],
  },
  {
    termEn: "toothbrush",
    termEs: "cepillo de dientes",
    canonical: "toothbrush",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["toothbrushes", "cepillos de dientes", "cepillo dental"],
  },
  {
    termEn: "mouthwash",
    termEs: "enjuague bucal",
    canonical: "mouthwash",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["mouth rinse", "enjuague dental"],
  },
  {
    termEn: "dental floss",
    termEs: "hilo dental",
    canonical: "dental floss",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["floss"],
  },
  {
    termEn: "deodorant",
    termEs: "desodorante",
    canonical: "deodorant",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["deodorants", "desodorantes", "antitranspirante"],
  },
  {
    termEn: "razor",
    termEs: "rastrillo",
    canonical: "razor",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["razors", "rastrillos", "navaja de afeitar", "maquinilla"],
  },
  {
    termEn: "shaving cream",
    termEs: "crema de afeitar",
    canonical: "shaving cream",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["shaving foam", "espuma de afeitar"],
  },
  {
    termEn: "lotion",
    termEs: "crema corporal",
    canonical: "lotion",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["body lotion", "crema humectante", "locion corporal"],
  },
  {
    termEn: "sunscreen",
    termEs: "protector solar",
    canonical: "sunscreen",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["sunblock", "bloqueador solar", "bronceador"],
  },
  {
    termEn: "hand sanitizer",
    termEs: "gel antibacterial",
    canonical: "hand sanitizer",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["sanitizer", "desinfectante de manos"],
  },
  {
    termEn: "cotton balls",
    termEs: "bolitas de algodon",
    canonical: "cotton balls",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["cotton", "algodon"],
  },
  {
    termEn: "cotton swabs",
    termEs: "hisopos",
    canonical: "cotton swabs",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["q tips", "qtips", "cotonetes", "bastoncillos"],
  },
  {
    termEn: "band aids",
    termEs: "curitas",
    canonical: "band aids",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["bandages", "bandaids", "tiritas", "venditas"],
  },
  {
    termEn: "feminine products",
    termEs: "productos femeninos",
    canonical: "feminine products",
    categoryId: CATEGORY_IDS.TOILETRIES,
    variants: ["tampons", "pads", "toallas sanitarias", "tampones"],
  },
] as const;

// Type for normalized lookup keys
type NormalizedKey = string;

// Build lookup maps for O(1) access
function buildLookupMaps(): {
  byEnglish: Map<NormalizedKey, DictionaryEntry>;
  bySpanish: Map<NormalizedKey, DictionaryEntry>;
  byVariant: Map<NormalizedKey, DictionaryEntry>;
} {
  const byEnglish = new Map<NormalizedKey, DictionaryEntry>();
  const bySpanish = new Map<NormalizedKey, DictionaryEntry>();
  const byVariant = new Map<NormalizedKey, DictionaryEntry>();

  for (const entry of DICTIONARY) {
    // Normalize keys for case-insensitive lookup
    const normalizedEn = entry.termEn.toLowerCase();
    const normalizedEs = entry.termEs.toLowerCase();

    byEnglish.set(normalizedEn, entry);
    bySpanish.set(normalizedEs, entry);

    // Add all variants to the variant map
    if (entry.variants) {
      for (const variant of entry.variants) {
        byVariant.set(variant.toLowerCase(), entry);
      }
    }
  }

  return { byEnglish, bySpanish, byVariant };
}

const lookupMaps = buildLookupMaps();

/**
 * Lookup a dictionary entry by English term.
 * @param term - English term to look up (case-insensitive)
 * @returns The dictionary entry or undefined if not found
 */
export function lookupByEnglish(term: string): DictionaryEntry | undefined {
  return lookupMaps.byEnglish.get(term.toLowerCase());
}

/**
 * Lookup a dictionary entry by Spanish term.
 * @param term - Spanish term to look up (case-insensitive)
 * @returns The dictionary entry or undefined if not found
 */
export function lookupBySpanish(term: string): DictionaryEntry | undefined {
  return lookupMaps.bySpanish.get(term.toLowerCase());
}

/**
 * Lookup a dictionary entry by any variant.
 * @param variant - Variant term to look up (case-insensitive)
 * @returns The dictionary entry or undefined if not found
 */
export function lookupByVariant(variant: string): DictionaryEntry | undefined {
  return lookupMaps.byVariant.get(variant.toLowerCase());
}

/**
 * Lookup a dictionary entry by any term (English, Spanish, or variant).
 * Checks in order: English, Spanish, then variants.
 * @param term - Term to look up (case-insensitive)
 * @returns The dictionary entry or undefined if not found
 */
export function lookupTerm(term: string): DictionaryEntry | undefined {
  const normalized = term.toLowerCase();
  return (
    lookupMaps.byEnglish.get(normalized) ??
    lookupMaps.bySpanish.get(normalized) ??
    lookupMaps.byVariant.get(normalized)
  );
}

/**
 * Get all dictionary entries for a specific category.
 * @param categoryId - The category ID to filter by
 * @returns Array of dictionary entries for that category
 */
export function getEntriesByCategory(
  categoryId: CategoryId
): DictionaryEntry[] {
  return DICTIONARY.filter((entry) => entry.categoryId === categoryId);
}

/**
 * Get dictionary statistics.
 * @returns Object with total count and count per category
 */
export function getDictionaryStats(): {
  total: number;
  byCategory: Record<string, number>;
} {
  const byCategory: Record<string, number> = {};

  for (const entry of DICTIONARY) {
    byCategory[entry.categoryId] = (byCategory[entry.categoryId] || 0) + 1;
  }

  return {
    total: DICTIONARY.length,
    byCategory,
  };
}
