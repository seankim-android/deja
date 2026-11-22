package dev.walkcache.checkout;

import android.app.Activity;
import android.graphics.Color;
import android.os.Bundle;
import android.provider.Settings;
import android.text.InputType;
import android.view.View;
import android.view.ViewGroup;
import android.widget.Button;
import android.widget.EditText;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

/** A deterministic, dependency-free Android app used only by the WalkCache PoC. */
public final class MainActivity extends Activity {
    private static final String CHECKOUT_SELECTOR_STALE_SETTING = "walkcache_checkout_selector_stale";
    private static final String PRODUCT_REGRESSION_SETTING = "walkcache_product_regression";
    private EditText usernameInput;
    private EditText passwordInput;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        showLogin();
    }

    private void showLogin() {
        LinearLayout content = screen();
        content.addView(title("WalkCache Shop", R.id.screen_login));
        content.addView(body("A tiny fixture store for agent-led QA."));

        usernameInput = input("Email", R.id.username_input, InputType.TYPE_TEXT_VARIATION_EMAIL_ADDRESS);
        passwordInput = input("Password", R.id.password_input, InputType.TYPE_TEXT_VARIATION_PASSWORD);
        content.addView(usernameInput);
        content.addView(passwordInput);

        Button login = button("Sign in", R.id.login_button);
        login.setOnClickListener(view -> {
            if (usernameInput.getText().toString().trim().isEmpty()) {
                usernameInput.setError("Enter the test email");
                return;
            }
            if (passwordInput.getText().length() == 0) {
                passwordInput.setError("Enter the test password");
                return;
            }
            showCatalog();
        });
        content.addView(login);
        setContentView(scroll(content));
    }

    private void showCatalog() {
        LinearLayout content = screen();
        content.addView(title("Catalog", R.id.screen_catalog));
        content.addView(body("Mochi sticker pack"));
        content.addView(body("$5.00 · 6 stickers"));

        Button add = button("Add to cart", R.id.add_to_cart);
        add.setOnClickListener(view -> showCart());
        content.addView(add);
        setContentView(scroll(content));
    }

    private void showCart() {
        LinearLayout content = screen();
        content.addView(title("Your cart", R.id.screen_cart));
        content.addView(body("1 × Mochi sticker pack"));
        content.addView(body("Total: $5.00"));

        Integer checkoutId = Settings.Global.getInt(
            getContentResolver(), CHECKOUT_SELECTOR_STALE_SETTING, 0
        ) == 1 ? null : R.id.checkout_button;
        Button checkout = button("Checkout", checkoutId);
        checkout.setOnClickListener(view -> showConfirmation());
        content.addView(checkout);
        setContentView(scroll(content));
    }

    private void showConfirmation() {
        LinearLayout content = screen();
        content.addView(title("Order confirmed", R.id.order_confirmation));
        String orderNumber = Settings.Global.getInt(
            getContentResolver(), PRODUCT_REGRESSION_SETTING, 0
        ) == 1 ? "WC-9999" : "WC-1001";
        TextView confirmation = body("Order #" + orderNumber + " · Mochi sticker pack");
        confirmation.setId(R.id.screen_confirmation);
        content.addView(confirmation);
        setContentView(scroll(content));
    }

    private ScrollView scroll(View content) {
        ScrollView scroll = new ScrollView(this);
        scroll.setFillViewport(true);
        scroll.addView(content);
        return scroll;
    }

    private LinearLayout screen() {
        LinearLayout layout = new LinearLayout(this);
        layout.setOrientation(LinearLayout.VERTICAL);
        layout.setPadding(dp(24), dp(40), dp(24), dp(24));
        layout.setBackgroundColor(Color.rgb(250, 247, 242));
        return layout;
    }

    private TextView title(String text, int id) {
        TextView view = new TextView(this);
        view.setId(id);
        view.setText(text);
        view.setTextSize(28);
        view.setTextColor(Color.rgb(35, 37, 41));
        view.setPadding(0, 0, 0, dp(24));
        return view;
    }

    private TextView body(String text) {
        TextView view = new TextView(this);
        view.setText(text);
        view.setTextSize(18);
        view.setTextColor(Color.rgb(55, 58, 64));
        view.setPadding(0, 0, 0, dp(18));
        return view;
    }

    private EditText input(String hint, int id, int variation) {
        EditText input = new EditText(this);
        input.setId(id);
        input.setContentDescription(hint);
        input.setHint(hint);
        input.setSingleLine(true);
        input.setTextSize(18);
        input.setInputType(InputType.TYPE_CLASS_TEXT | variation);
        input.setLayoutParams(margins());
        return input;
    }

    private Button button(String label, Integer id) {
        Button button = new Button(this);
        if (id != null) {
            button.setId(id);
        }
        button.setText(label);
        button.setAllCaps(false);
        button.setTextSize(18);
        button.setLayoutParams(margins());
        return button;
    }

    private LinearLayout.LayoutParams margins() {
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(
            ViewGroup.LayoutParams.MATCH_PARENT,
            ViewGroup.LayoutParams.WRAP_CONTENT
        );
        params.bottomMargin = dp(16);
        return params;
    }

    private int dp(int value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }
}

