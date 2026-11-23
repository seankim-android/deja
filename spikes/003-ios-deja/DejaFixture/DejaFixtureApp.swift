import SwiftUI

@main
struct DejaFixtureApp: App {
    init() {
        FirstLaunchCrash.crashIfRequested()
    }

    var body: some Scene {
        WindowGroup {
            CheckoutView()
        }
    }
}

/// DEJA-7 evidence mode, off unless the simulator-global default `DEJA_FIXTURE_CRASH_MARKER` names a
/// file path. The first launch finds no file, creates it and crashes; every later launch finds it
/// and starts normally. The marker lives outside the app container on purpose: the flow's
/// `clearState: true` wipes the container, which would make every launch crash.
private enum FirstLaunchCrash {
    static func crashIfRequested() {
        guard let marker = UserDefaults.standard.string(forKey: "DEJA_FIXTURE_CRASH_MARKER"),
              !marker.isEmpty else { return }
        if FileManager.default.fileExists(atPath: marker) { return }
        FileManager.default.createFile(atPath: marker, contents: Data())
        fatalError("DejaFixture simulated crash on first launch")
    }
}

private enum Screen {
    case login
    case catalog
    case cart
    case confirmation
}

private struct CheckoutView: View {
    @State private var screen = Screen.login
    @State private var email = ""
    @State private var password = ""
    @State private var loginError = ""

    private var fixtureOrder: String {
        UserDefaults.standard.string(forKey: "DEJA_FIXTURE_ORDER") ?? "WC-1001"
    }

    var body: some View {
        VStack(spacing: 20) {
            Text("Déjà iOS Shop")
                .font(.largeTitle)
                .accessibilityIdentifier("shop_title")

            switch screen {
            case .login:
                login
            case .catalog:
                catalog
            case .cart:
                cart
            case .confirmation:
                confirmation
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .padding(24)
        .background(Color(.systemBackground))
    }

    private var login: some View {
        VStack(spacing: 16) {
            Text("Fixture login")
                .font(.title2)
                .accessibilityIdentifier("screen_login")
            TextField("Fixture email", text: $email)
                .textFieldStyle(.roundedBorder)
                .keyboardType(.emailAddress)
                .textContentType(.emailAddress)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .accessibilityIdentifier("username_input")
            SecureField("Fixture password", text: $password)
                .textFieldStyle(.roundedBorder)
                .textContentType(.password)
                .accessibilityIdentifier("password_input")
            if !loginError.isEmpty {
                Text(loginError)
                    .foregroundStyle(.red)
                    .accessibilityIdentifier("login_error")
            }
            Button("Log in") {
                if email == "qa@example.com" && password == "testing123" {
                    loginError = ""
                    screen = .catalog
                } else {
                    loginError = "Use the deterministic fixture credentials"
                }
            }
            .buttonStyle(.borderedProminent)
            .accessibilityIdentifier("login_button")
        }
    }

    private var catalog: some View {
        VStack(spacing: 16) {
            Text("Catalog")
                .font(.title2)
                .accessibilityIdentifier("screen_catalog")
            Text("Mochi sticker pack")
                .accessibilityIdentifier("product_name")
            Text("$5.00 · 6 stickers")
                .accessibilityIdentifier("product_details")
            Button("Add to cart") {
                screen = .cart
            }
            .buttonStyle(.borderedProminent)
            .accessibilityIdentifier("add_to_cart")
        }
    }

    private var cart: some View {
        VStack(spacing: 16) {
            Text("Cart")
                .font(.title2)
                .accessibilityIdentifier("screen_cart")
            Text("1 × Mochi sticker pack")
                .accessibilityIdentifier("cart_line")
            Text("Total: $5.00")
                .accessibilityIdentifier("cart_total")
            Button("Checkout") {
                screen = .confirmation
            }
            .buttonStyle(.borderedProminent)
            .accessibilityIdentifier("checkout_button")
        }
    }

    private var confirmation: some View {
        VStack(spacing: 16) {
            Text("Confirmation")
                .font(.title2)
                .accessibilityIdentifier("order_confirmation")
            Text("Order confirmed")
                .accessibilityIdentifier("order_status")
            Text("Order #\(fixtureOrder) · Mochi sticker pack")
                .accessibilityIdentifier("order_details")
        }
    }
}

#Preview {
    CheckoutView()
}
